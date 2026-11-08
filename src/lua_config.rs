use mlua::{
    Function, HookTriggers, Lua, LuaOptions, MultiValue, Result as LuaResult, StdLib, Table, Value,
    VmState,
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    error::Error,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Debug, Clone)]
enum LuaValue {
    Scalar(String),
    Table(Vec<(Option<String>, LuaValue)>),
}

pub fn is_lua_config_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("lua")
}

pub fn has_hyprviz_require(source: &str) -> bool {
    require_modules(source).iter().any(|module| is_hyprviz_module(module))
}

/// True only when the line consists of a standalone HyprViz require statement.
/// This lets the GUI hide/switch its managed include without deleting other Lua
/// statements that happen to share a line with a require call.
pub fn is_hyprviz_require_line(source: &str) -> bool {
    let line = source.trim();
    let bytes = line.as_bytes();
    if !line.starts_with("require") {
        return false;
    }

    let mut cursor = "require".len();
    if bytes.get(cursor).is_some_and(|byte| {
        byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'.'
    }) {
        return false;
    }
    while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
        cursor += 1;
    }

    let parenthesized = if bytes.get(cursor) == Some(&b'(') {
        cursor += 1;
        while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
            cursor += 1;
        }
        true
    } else {
        false
    };

    let Some(module) = read_lua_string(line, &mut cursor) else {
        return false;
    };
    if !is_hyprviz_module(&module) {
        return false;
    }

    while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
        cursor += 1;
    }
    if parenthesized {
        if bytes.get(cursor) != Some(&b')') {
            return false;
        }
        cursor += 1;
    }
    while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b';') {
        cursor += 1;
        while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_whitespace()) {
            cursor += 1;
        }
    }

    cursor == bytes.len()
        || line[cursor..].starts_with("--")
}

pub fn append_hyprviz_require(source: &str) -> String {
    if has_hyprviz_require(source) {
        return source.to_string();
    }
    let mut result = source.trim_end().to_string();
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str("require(\"./hyprviz\")\n");
    result
}

pub fn profile_name_from_lua(source: &str) -> Option<String> {
    let mut profile = None;
    for module in require_modules(source) {
        if module == "./hyprviz" || module == "hyprviz" {
            profile = Some("Default".to_string());
        } else if let Some(name) = module
            .strip_prefix("./hyprviz/")
            .or_else(|| module.strip_prefix("hyprviz/"))
        {
            let name = name.trim_end_matches(".lua");
            if !name.is_empty() && !name.contains('/') {
                profile = Some(name.to_string());
            }
        }
    }
    profile
}

/// Execute a real Lua 5.4 chunk and collect declarations made through the
/// Hyprland configuration API shim. Lua control flow, functions, locals,
/// computed values, and module loading are handled by the Lua VM.
pub fn expand_lua_config(
    entry_path: &Path,
    entry: Option<&str>,
    exclude_hyprviz: bool,
) -> Result<String, Box<dyn Error>> {
    execute_lua_config(entry_path, entry, exclude_hyprviz)
}

#[allow(dead_code)]
pub fn lua_to_hyprlang(source: &str) -> String {
    let path = Path::new(".config/hypr/hyprland.lua");
    expand_lua_config(path, Some(source), false)
        .unwrap_or_else(|error| format!("# HyprViz Lua evaluation failed: {error}\n"))
}

fn execute_lua_config(
    entry_path: &Path,
    entry: Option<&str>,
    exclude_hyprviz: bool,
) -> Result<String, Box<dyn Error>> {
    let config_dir = entry_path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let source = match entry {
        Some(source) => source.to_string(),
        None => fs::read_to_string(entry_path)?,
    };

    // Keep the complete Lua language/runtime, but remove general-purpose IO.
    // The editor loads config modules through the restricted loader below.
    // Preserve read-only OS helpers commonly used by configs (not execute/remove/rename).
    let lua = Lua::new_with(StdLib::ALL_SAFE & !StdLib::IO, LuaOptions::new())?;
    // Keep pathological table/string allocations from exhausting the host process.
    lua.set_memory_limit(128 * 1024 * 1024)?;
    let globals = lua.globals();
    let os: Table = globals.get("os")?;
    let safe_os = lua.create_table()?;
    for name in ["clock", "date", "difftime", "getenv", "time"] {
        let value: Value = os.get(name)?;
        safe_os.set(name, value)?;
    }
    globals.set("os", safe_os.clone())?;
    let package: Table = globals.get("package")?;
    let loaded_modules: Table = package.get("loaded")?;
    loaded_modules.set("os", safe_os)?;

    // A malformed or accidental infinite loop must not hang the GTK main thread.
    // Count VM instructions across the main chunk and Lua-created coroutines.
    let executed_instructions = Rc::new(Cell::new(0u64));
    let instruction_counter = Rc::clone(&executed_instructions);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            let executed = instruction_counter.get().saturating_add(10_000);
            instruction_counter.set(executed);
            if executed > 25_000_000 {
                return Err(mlua::Error::RuntimeError(
                    "Lua configuration exceeded the 25 million instruction evaluation limit"
                        .to_string(),
                ));
            }
            Ok(VmState::Continue)
        },
    )?;
    let output = Rc::new(RefCell::new(Vec::<String>::new()));
    register_hyprland_api(&lua, Rc::clone(&output))?;

    let module_stack = Rc::new(RefCell::new(vec![entry_path.to_path_buf()]));
    let globals = lua.globals();
    let native_require: Function = globals.get("require")?;
    let package: Table = globals.get("package")?;
    let old_package_path: String = package.get("path")?;
    package.set(
        "path",
        format!(
            "{};{};{}",
            config_dir.join("?.lua").display(),
            config_dir.join("?/init.lua").display(),
            old_package_path
        ),
    )?;

    let loaded: Table = package.get("loaded")?;
    let package_searchpath: Function = package.get("searchpath")?;
    let require_package = package.clone();
    let require_stack = Rc::clone(&module_stack);
    let require_config_dir = config_dir.clone();
    let require_fn = lua.create_function(move |lua, module: String| {
        let cached: Value = loaded.get(module.as_str())?;
        if !matches!(&cached, Value::Nil | Value::Boolean(false)) {
            return Ok(cached);
        }

        if exclude_hyprviz && is_hyprviz_module(&module) {
            loaded.set(module, true)?;
            return Ok(Value::Boolean(true));
        }

        let caller = require_stack
            .borrow()
            .last()
            .cloned()
            .unwrap_or_else(|| require_config_dir.join("hyprland.lua"));

        let module_path = resolve_module_path(&module, &caller, &require_config_dir)
            .or_else(|| {
                let package_path: String = require_package.get("path").ok()?;
                package_searchpath
                    .call::<Option<String>>((module.as_str(), package_path))
                    .ok()
                    .flatten()
                    .map(PathBuf::from)
            });
        let Some(module_path) = module_path else {
            return native_require.call::<Value>(module);
        };

        loaded.set(module.as_str(), true)?;
        let contents = match fs::read_to_string(&module_path) {
            Ok(contents) => contents,
            Err(error) => {
                loaded.set(module.as_str(), Value::Nil)?;
                return Err(mlua::Error::external(error));
            }
        };

        require_stack.borrow_mut().push(module_path.clone());
        let evaluation = lua
            .load(&contents)
            .set_name(module_path.to_string_lossy().into_owned())
            .eval::<Value>();
        require_stack.borrow_mut().pop();

        match evaluation {
            Ok(Value::Nil) => {
                loaded.set(module, true)?;
                Ok(Value::Boolean(true))
            }
            Ok(value) => {
                loaded.set(module, value.clone())?;
                Ok(value)
            }
            Err(error) => {
                loaded.set(module, Value::Nil)?;
                Err(error)
            }
        }
    })?;
    globals.set("require", require_fn)?;

    let dofile_stack = Rc::clone(&module_stack);
    let dofile_config_dir = config_dir.clone();
    let dofile = lua.create_function(move |lua, filename: String| {
        let caller = dofile_stack
            .borrow()
            .last()
            .cloned()
            .unwrap_or_else(|| dofile_config_dir.join("hyprland.lua"));
        let path = resolve_module_path(&filename, &caller, &dofile_config_dir)
            .ok_or_else(|| mlua::Error::RuntimeError(format!("Lua file not found: {filename}")))?;
        let contents = fs::read_to_string(&path).map_err(mlua::Error::external)?;
        dofile_stack.borrow_mut().push(path.clone());
        let result = lua
            .load(&contents)
            .set_name(path.to_string_lossy().into_owned())
            .eval::<MultiValue>();
        dofile_stack.borrow_mut().pop();
        result
    })?;
    globals.set("dofile", dofile)?;

    let loadfile_stack = Rc::clone(&module_stack);
    let loadfile_config_dir = config_dir.clone();
    let loadfile = lua.create_function(move |lua, filename: String| {
        let caller = loadfile_stack
            .borrow()
            .last()
            .cloned()
            .unwrap_or_else(|| loadfile_config_dir.join("hyprland.lua"));
        let path = resolve_module_path(&filename, &caller, &loadfile_config_dir)
            .ok_or_else(|| mlua::Error::RuntimeError(format!("Lua file not found: {filename}")))?;
        let contents = fs::read_to_string(&path).map_err(mlua::Error::external)?;
        let chunk = lua
            .load(&contents)
            .set_name(path.to_string_lossy().into_owned())
            .into_function()?;
        let chunk_path = path.clone();
        let chunk_stack = Rc::clone(&loadfile_stack);
        lua.create_function(move |_, args: MultiValue| {
            chunk_stack.borrow_mut().push(chunk_path.clone());
            let result = chunk.call::<MultiValue>(args);
            chunk_stack.borrow_mut().pop();
            result
        })
    })?;
    globals.set("loadfile", loadfile)?;

    lua.load(&source)
        .set_name(entry_path.to_string_lossy().into_owned())
        .exec()?;

    let result = output.borrow().join("\n");
    Ok(if result.is_empty() {
        String::new()
    } else {
        result + "\n"
    })
}

fn resolve_module_path(module: &str, caller: &Path, config_dir: &Path) -> Option<PathBuf> {
    let caller_dir = caller.parent().unwrap_or(config_dir);
    let raw = PathBuf::from(module);
    let relative = if raw.is_absolute() {
        raw
    } else if module.starts_with("./") || module.starts_with("../") {
        caller_dir.join(raw)
    } else {
        let module_path = if module.ends_with(".lua") || module.contains('/') {
            module.to_string()
        } else {
            module.replace('.', "/")
        };
        config_dir.join(module_path)
    };

    let mut candidates = vec![relative.clone()];
    if relative.extension().is_none() {
        candidates.push(relative.with_extension("lua"));
    }
    candidates.push(relative.join("init.lua"));
    candidates.into_iter().find(|path| path.is_file())
}

fn require_modules(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut modules = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] == b'-' && bytes.get(cursor + 1) == Some(&b'-') {
            cursor += 2;
            if let Some(end) = skip_lua_long_bracket(source, cursor) {
                cursor = end;
            } else {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            continue;
        }

        if bytes[cursor] == b'\'' || bytes[cursor] == b'"' {
            skip_lua_string(source, &mut cursor);
            continue;
        }

        if bytes[cursor] == b'['
            && let Some(end) = skip_lua_long_bracket(source, cursor)
        {
            cursor = end;
            continue;
        }

        if bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_' {
            let start = cursor;
            cursor += 1;
            while bytes.get(cursor).is_some_and(|byte| {
                byte.is_ascii_alphanumeric() || *byte == b'_'
            }) {
                cursor += 1;
            }
            if &source[start..cursor] == "require"
                && (start == 0 || bytes[start - 1] != b'.')
            {
                let mut argument = cursor;
                while bytes.get(argument).is_some_and(|byte| byte.is_ascii_whitespace()) {
                    argument += 1;
                }
                if bytes.get(argument) == Some(&b'(') {
                    argument += 1;
                    while bytes.get(argument).is_some_and(|byte| byte.is_ascii_whitespace()) {
                        argument += 1;
                    }
                }
                if bytes.get(argument).is_some_and(|byte| *byte == b'\'' || *byte == b'"') {
                    if let Some(module) = read_lua_string(source, &mut argument) {
                        modules.push(module);
                    }
                }
            }
            continue;
        }
        cursor += 1;
    }
    modules
}

fn skip_lua_long_bracket(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }

    let mut cursor = start + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'[') {
        return None;
    }

    let equals = cursor - start - 1;
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b']' {
            let mut close = cursor + 1;
            let mut matched = 0;
            while matched < equals && bytes.get(close) == Some(&b'=') {
                close += 1;
                matched += 1;
            }
            if matched == equals && bytes.get(close) == Some(&b']') {
                return Some(close + 1);
            }
        }
        cursor += 1;
    }
    Some(bytes.len())
}

fn skip_lua_string(source: &str, cursor: &mut usize) {
    let _ = read_lua_string(source, cursor);
}

fn read_lua_string(source: &str, cursor: &mut usize) -> Option<String> {
    let quote = *source.as_bytes().get(*cursor)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    *cursor += 1;
    let mut output = String::new();
    while *cursor < source.len() {
        let byte = source.as_bytes()[*cursor];
        *cursor += 1;
        if byte == quote {
            return Some(output);
        }
        if byte == b'\\' {
            let escaped = *source.as_bytes().get(*cursor)?;
            *cursor += 1;
            output.push(match escaped {
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'\\' => '\\',
                b'\'' => '\'',
                b'"' => '"',
                other => other as char,
            });
        } else {
            let start = *cursor - 1;
            let ch = source[start..].chars().next()?;
            output.push(ch);
            *cursor = start + ch.len_utf8();
        }
    }
    None
}

fn is_hyprviz_module(module: &str) -> bool {
    module == "./hyprviz"
        || module == "hyprviz"
        || module.starts_with("./hyprviz/")
        || module.starts_with("hyprviz/")
}

fn register_hyprland_api(lua: &Lua, output: Rc<RefCell<Vec<String>>>) -> LuaResult<()> {
    let hl = lua.create_table()?;
    let known_config = Rc::new(RefCell::new(HashMap::<String, LuaValue>::new()));

    let config_output = Rc::clone(&output);
    let config_values = Rc::clone(&known_config);
    hl.set(
        "config",
        lua.create_function(move |_, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            let mut lines = Vec::new();
            emit_config_table(&value, &mut Vec::new(), &mut lines);
            append_output(&config_output, lines.join("\n"));
            flatten_config_values(&value, "", &mut *config_values.borrow_mut());
            Ok(())
        })?,
    )?;

    let get_config_values = Rc::clone(&known_config);
    hl.set(
        "get_config",
        lua.create_function(move |lua, key: String| {
            let key = key.replace(':', ".");
            match get_config_values.borrow().get(&key).cloned() {
                Some(value) => lua_value_to_ml_value(lua, &value, 0),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let monitor_output = Rc::clone(&output);
    hl.set(
        "monitor",
        lua.create_function(move |_, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = monitor_to_hyprlang(&value) {
                append_output(&monitor_output, line);
            }
            Ok(())
        })?,
    )?;

    let env_output = Rc::clone(&output);
    hl.set(
        "env",
        lua.create_function(move |_, args: MultiValue| {
            let mut args = args.into_iter();
            let name = args.next().as_ref().and_then(lua_value_as_string);
            let value = args.next().as_ref().and_then(lua_value_as_string);
            if let (Some(name), Some(value)) = (name, value) {
                let value = if value.contains(',') || value.contains('#') {
                    format!(
                        "\"{}\"",
                        value.replace('\\', "\\\\").replace('"', "\\\"")
                    )
                } else {
                    value
                };
                append_output(&env_output, format!("env = {name},{value}"));
            }
            Ok(())
        })?,
    )?;

    let bind_output = Rc::clone(&output);
    hl.set(
        "bind",
        lua.create_function(move |lua, args: MultiValue| {
            let mut args = args.into_iter();
            let key = args.next().as_ref().and_then(lua_value_as_string);
            let dispatcher = args.next();
            let options = args.next();
            if let (Some(key), Some(dispatcher)) = (key, dispatcher)
                && let Some(line) = bind_to_hyprlang(&key, dispatcher, options)
            {
                append_output(&bind_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let unbind_output = Rc::clone(&output);
    hl.set(
        "unbind",
        lua.create_function(move |_, key: String| {
            let key_parts = key
                .split(['+', ' '])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            if let Some(key_name) = key_parts.last() {
                let modifiers = key_parts[..key_parts.len() - 1].join("_");
                append_output(&unbind_output, format!("unbind = {modifiers},{key_name}"));
            }
            Ok(())
        })?,
    )?;

    let rule_output = Rc::clone(&output);
    hl.set(
        "window_rule",
        lua.create_function(move |lua, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = window_rule_to_hyprlang(&value) {
                append_output(&rule_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let layer_output = Rc::clone(&output);
    hl.set(
        "layer_rule",
        lua.create_function(move |lua, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = layer_rule_to_hyprlang(&value) {
                append_output(&layer_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let workspace_output = Rc::clone(&output);
    hl.set(
        "workspace_rule",
        lua.create_function(move |lua, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = workspace_rule_to_hyprlang(&value) {
                append_output(&workspace_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let animation_output = Rc::clone(&output);
    hl.set(
        "animation",
        lua.create_function(move |lua, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = animation_to_hyprlang(&value) {
                append_output(&animation_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let gesture_output = Rc::clone(&output);
    hl.set(
        "gesture",
        lua.create_function(move |lua, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = gesture_to_hyprlang(&value) {
                append_output(&gesture_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let curve_output = Rc::clone(&output);
    hl.set(
        "curve",
        lua.create_function(move |_, (name, definition): (String, Table)| {
            let value = lua_table_to_value(definition, 0)?;
            if let Some(line) = curve_to_hyprlang(&name, &value) {
                append_output(&curve_output, line);
            }
            Ok(())
        })?,
    )?;

    let device_output = Rc::clone(&output);
    hl.set(
        "device",
        lua.create_function(move |lua, table: Table| {
            let value = lua_table_to_value(table, 0)?;
            if let Some(line) = generic_section_to_hyprlang("device", &value, "name") {
                append_output(&device_output, line);
            }
            create_handle(lua)
        })?,
    )?;

    let permission_output = Rc::clone(&output);
    hl.set(
        "permission",
        lua.create_function(move |_, args: MultiValue| {
            let mut args = args.into_iter();
            let first = args.next();
            let line = match first {
                Some(Value::Table(table)) => {
                    let value = lua_table_to_value(table, 0)?;
                    let binary = table_string(&value, "binary");
                    let kind = table_string(&value, "type");
                    let mode = table_string(&value, "mode");
                    match (binary, kind, mode) {
                        (Some(binary), Some(kind), Some(mode)) => {
                            Some(format!("permission = {binary},{kind},{mode}"))
                        }
                        _ => None,
                    }
                }
                Some(binary) => {
                    let binary = lua_value_as_string(&binary);
                    let kind = args.next().as_ref().and_then(lua_value_as_string);
                    let mode = args.next().as_ref().and_then(lua_value_as_string);
                    match (binary, kind, mode) {
                        (Some(binary), Some(kind), Some(mode)) => {
                            Some(format!("permission = {binary},{kind},{mode}"))
                        }
                        _ => None,
                    }
                }
                None => None,
            };
            if let Some(line) = line {
                append_output(&permission_output, line);
            }
            Ok(())
        })?,
    )?;

    let startup_callback_depth = Rc::new(Cell::new(0usize));
    let exec_output = Rc::clone(&output);
    let exec_startup_depth = Rc::clone(&startup_callback_depth);
    hl.set(
        "exec_cmd",
        lua.create_function(move |_, args: MultiValue| {
            if let Some(command) = args.into_iter().next().as_ref().and_then(lua_value_as_string) {
                let directive = if exec_startup_depth.get() > 0 {
                    "exec-once"
                } else {
                    "exec"
                };
                append_output(&exec_output, format!("{directive} = {command}"));
            }
            Ok(())
        })?,
    )?;

    // Do not run compositor commands, timers, or plugins while browsing a config.
    // The startup event callback is evaluated once so its declarative exec_cmd
    // calls can be shown in the UI; other event callbacks remain inert.
    for name in [
        "dispatch",
        "print",
        "off",
        "load_plugin",
        "clear_crashed_lockscreen",
        "exec_scheduled_prop_refresh_immediately",
    ] {
        hl.set(name, lua.create_function(|_, _: MultiValue| Ok(()))?)?;
    }

    let startup_event_depth = Rc::clone(&startup_callback_depth);
    hl.set(
        "on",
        lua.create_function(move |lua, args: MultiValue| {
            let mut args = args.into_iter();
            let event = args.next().as_ref().and_then(lua_value_as_string);
            let callback = args.find_map(|value| match value {
                Value::Function(function) => Some(function),
                _ => None,
            });
            if event.as_deref() == Some("hyprland.start")
                && let Some(callback) = callback
            {
                startup_event_depth.set(startup_event_depth.get().saturating_add(1));
                let result = callback.call::<()>(());
                startup_event_depth.set(startup_event_depth.get().saturating_sub(1));
                result?;
            }
            create_handle(lua)
        })?,
    )?;
    hl.set(
        "timer",
        lua.create_function(|lua, _: MultiValue| create_handle(lua))?,
    )?;
    hl.set(
        "define_submap",
        // The callback is retained in the user's Lua source, but is not invoked
        // here: it belongs to live compositor input state and can register binds.
        lua.create_function(|lua, _: MultiValue| create_handle(lua))?,
    )?;
    hl.set(
        "is_key_down",
        lua.create_function(|_, _: MultiValue| Ok(false))?,
    )?;
    hl.set(
        "version",
        lua.create_function(|_, _: MultiValue| Ok("unknown"))?,
    )?;

    for name in [
        "get_windows",
        "get_workspace_windows",
        "get_workspaces",
        "get_monitors",
        "get_layers",
        "get_loaded_plugins",
    ] {
        hl.set(name, lua.create_function(|lua, _: MultiValue| lua.create_table())?)?;
    }
    for name in [
        "get_window",
        "get_active_window",
        "get_urgent_window",
        "get_last_window",
        "get_workspace",
        "get_active_workspace",
        "get_active_special_workspace",
        "get_last_workspace",
        "get_monitor",
        "get_active_monitor",
        "get_monitor_at",
        "get_monitor_at_cursor",
        "get_cursor_pos",
        "get_current_submap",
    ] {
        hl.set(name, lua.create_function(|_, _: MultiValue| Ok(Value::Nil))?)?;
    }

    hl.set("dsp", create_dispatcher_namespace(lua, "hl.dsp".to_string())?)?;
    hl.set("notification", create_noop_namespace(lua)?)?;
    hl.set("plugin", create_noop_namespace(lua)?)?;
    hl.set("layout", create_noop_namespace(lua)?)?;

    let meta = lua.create_table()?;
    meta.set(
        "__index",
        lua.create_function(|lua, (_table, _key): (Table, Value)| {
            let function = lua.create_function(|_, _: MultiValue| Ok(()))?;
            Ok(Value::Function(function))
        })?,
    )?;
    hl.set_metatable(Some(meta))?;
    lua.globals().set("hl", hl)?;
    Ok(())
}

fn create_handle(lua: &Lua) -> LuaResult<Table> {
    let handle = lua.create_table()?;
    for name in [
        "set_enabled",
        "remove",
        "cancel",
        "dismiss",
        "pause",
        "resume",
        "set_paused",
        "set_text",
        "set_timeout",
        "set_color",
        "set_icon",
        "set_font_size",
    ] {
        handle.set(name, lua.create_function(|_, _: MultiValue| Ok(()))?)?;
    }
    handle.set("is_active", lua.create_function(|_, _: MultiValue| Ok(true))?)?;
    handle.set("is_alive", lua.create_function(|_, _: MultiValue| Ok(true))?)?;
    handle.set("is_paused", lua.create_function(|_, _: MultiValue| Ok(false))?)?;
    handle.set("get_text", lua.create_function(|_, _: MultiValue| Ok(""))?)?;
    Ok(handle)
}

fn create_noop_namespace(lua: &Lua) -> LuaResult<Table> {
    let namespace = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set(
        "__index",
        lua.create_function(|lua, (_table, _key): (Table, Value)| {
            let function = lua.create_function(|lua, _: MultiValue| create_handle(lua))?;
            Ok(Value::Function(function))
        })?,
    )?;
    namespace.set_metatable(Some(metatable))?;
    Ok(namespace)
}

fn create_dispatcher_namespace(lua: &Lua, prefix: String) -> LuaResult<Table> {
    let namespace = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set(
        "__index",
        lua.create_function(move |lua, (_namespace, key): (Table, String)| {
            let full_name = format!("{prefix}.{key}");
            if is_dispatcher_namespace(&full_name) {
                Ok(Value::Table(create_dispatcher_namespace(lua, full_name)?))
            } else {
                let name = full_name;
                let function = lua.create_function(move |lua, args: MultiValue| {
                    let descriptor = lua.create_table()?;
                    descriptor.set("__hyprviz_dispatcher", name.as_str())?;
                    let argument_table = lua.create_table()?;
                    for (index, argument) in args.into_iter().enumerate() {
                        argument_table.set(index + 1, argument)?;
                    }
                    descriptor.set("args", argument_table)?;
                    Ok(descriptor)
                })?;
                Ok(Value::Function(function))
            }
        })?,
    )?;
    namespace.set_metatable(Some(metatable))?;
    Ok(namespace)
}

fn is_dispatcher_namespace(name: &str) -> bool {
    matches!(
        name,
        "hl.dsp.window"
            | "hl.dsp.workspace"
            | "hl.dsp.group"
            | "hl.dsp.cursor"
    )
}

fn append_output(output: &Rc<RefCell<Vec<String>>>, text: String) {
    let text = text.trim();
    if !text.is_empty() {
        output.borrow_mut().push(text.to_string());
    }
}

fn lua_table_to_value(table: Table, depth: usize) -> LuaResult<LuaValue> {
    lua_value_to_value(Value::Table(table), depth)
}

fn lua_value_to_value(value: Value, depth: usize) -> LuaResult<LuaValue> {
    if depth > 64 {
        return Ok(LuaValue::Scalar("<maximum Lua table depth>".to_string()));
    }
    match value {
        Value::Nil => Ok(LuaValue::Scalar("nil".to_string())),
        Value::Boolean(value) => Ok(LuaValue::Scalar(value.to_string())),
        Value::Integer(value) => Ok(LuaValue::Scalar(value.to_string())),
        Value::Number(value) => Ok(LuaValue::Scalar(value.to_string())),
        Value::String(value) => Ok(LuaValue::Scalar(value.to_string_lossy().to_string())),
        Value::Table(table) => {
            let sequence_len = table.raw_len();
            let mut entries = Vec::new();

            for index in 1..=sequence_len {
                let value: Value = table.raw_get(index)?;
                entries.push((None, lua_value_to_value(value, depth + 1)?));
            }

            for pair in table.clone().pairs::<Value, Value>() {
                let (key, value) = pair?;
                if let Value::Integer(index) = &key {
                    if *index >= 1 && (*index as usize) <= sequence_len {
                        continue;
                    }
                }
                let key = match key {
                    Value::String(key) => Some(key.to_string_lossy().to_string()),
                    Value::Integer(key) => Some(key.to_string()),
                    Value::Number(key) => Some(key.to_string()),
                    Value::Boolean(key) => Some(key.to_string()),
                    _ => None,
                };
                entries.push((key, lua_value_to_value(value, depth + 1)?));
            }
            Ok(LuaValue::Table(entries))
        }
        Value::Function(_) => Ok(LuaValue::Scalar("<Lua function>".to_string())),
        Value::Thread(_) => Ok(LuaValue::Scalar("<Lua thread>".to_string())),
        Value::UserData(_) => Ok(LuaValue::Scalar("<Lua userdata>".to_string())),
        Value::Error(error) => Ok(LuaValue::Scalar(error.to_string())),
        Value::LightUserData(_) => Ok(LuaValue::Scalar("<Lua pointer>".to_string())),
        _ => Ok(LuaValue::Scalar("<Lua value>".to_string())),
    }
}

fn lua_value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::Nil => None,
        Value::String(value) => Some(value.to_string_lossy().to_string()),
        Value::Boolean(value) => Some(value.to_string()),
        Value::Integer(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Table(table) => lua_table_to_value(table.clone(), 0)
            .ok()
            .and_then(|value| value_to_hyprlang(&value)),
        _ => None,
    }
}

fn lua_value_to_ml_value(lua: &Lua, value: &LuaValue, depth: usize) -> LuaResult<Value> {
    if depth > 64 {
        return Ok(Value::Nil);
    }
    match value {
        LuaValue::Scalar(value) => {
            let value = value.trim();
            if value == "nil" {
                Ok(Value::Nil)
            } else if value == "true" {
                Ok(Value::Boolean(true))
            } else if value == "false" {
                Ok(Value::Boolean(false))
            } else if let Ok(value) = value.parse::<i64>() {
                Ok(Value::Integer(value))
            } else if let Ok(value) = value.parse::<f64>() {
                Ok(Value::Number(value))
            } else {
                Ok(Value::String(lua.create_string(value)?))
            }
        }
        LuaValue::Table(entries) => {
            let table = lua.create_table()?;
            let mut sequence_index = 1usize;
            for (key, child) in entries {
                let child = lua_value_to_ml_value(lua, child, depth + 1)?;
                if let Some(key) = key {
                    table.set(key.as_str(), child)?;
                } else {
                    table.set(sequence_index, child)?;
                    sequence_index += 1;
                }
            }
            Ok(Value::Table(table))
        }
    }
}

fn flatten_config_values(
    value: &LuaValue,
    prefix: &str,
    values: &mut HashMap<String, LuaValue>,
) {
    if !prefix.is_empty() {
        values.insert(prefix.to_string(), value.clone());
    }
    let LuaValue::Table(entries) = value else {
        return;
    };
    for (key, child) in entries {
        let Some(key) = key.as_deref() else {
            continue;
        };
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        flatten_config_values(child, &path, values);
    }
}

fn monitor_to_hyprlang(value: &LuaValue) -> Option<String> {
    let output = table_string(value, "output")?;
    let mut fields = vec![
        output,
        table_string(value, "mode").unwrap_or_else(|| "preferred".to_string()),
        table_string(value, "position").unwrap_or_else(|| "auto".to_string()),
        table_string(value, "scale").unwrap_or_else(|| "auto".to_string()),
    ];

    for key in [
        "transform",
        "mirror",
        "bitdepth",
        "cm",
        "sdr_eotf",
        "sdrbrightness",
        "sdrsaturation",
        "vrr",
        "icc",
        "supports_wide_color",
        "supports_hdr",
        "sdr_min_luminance",
        "sdr_max_luminance",
        "min_luminance",
        "max_luminance",
        "max_avg_luminance",
        "reserved",
    ] {
        if let Some(value) = table_string(value, key) {
            fields.push(value);
        }
    }
    if table_bool(value, "disabled") == Some(true) {
        fields.push("disabled".to_string());
    }

    Some(format!("monitor = {}", fields.join(", ")))
}

fn bind_to_hyprlang(key: &str, dispatcher: Value, options: Option<Value>) -> Option<String> {
    let descriptor = match dispatcher {
        Value::Table(table) => lua_table_to_value(table, 0).ok()?,
        Value::Function(_) => return None,
        _ => return None,
    };
    let name = table_string(&descriptor, "__hyprviz_dispatcher")?;
    let args = table_field(&descriptor, "args")
        .and_then(as_array)
        .unwrap_or_default();

    let (dispatcher_name, mut dispatcher_args) = dispatcher_to_hyprland_args(&name, &args)?;
    let key_parts = key
        .split(['+', ' '])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let key_name = key_parts.last()?.to_string();
    let modifiers = key_parts[..key_parts.len() - 1].join("_");

    let mut directive = "bind".to_string();
    if let Some(options) = options.and_then(|value| match value {
        Value::Table(table) => lua_table_to_value(table, 0).ok(),
        _ => None,
    }) {
        if table_bool(&options, "locked") == Some(true) {
            directive.push('l');
        }
        if table_bool(&options, "repeating") == Some(true)
            || table_bool(&options, "repeat") == Some(true)
        {
            directive.push('e');
        }
        if table_bool(&options, "release") == Some(true) {
            directive.push('r');
        }
        if table_bool(&options, "mouse") == Some(true) {
            directive.push('m');
        }
        if table_bool(&options, "non_consuming") == Some(true) {
            directive.push('n');
        }
        if table_bool(&options, "submap_universal") == Some(true) {
            directive.push('s');
        }
    }

    let mut fields = vec![modifiers, key_name, dispatcher_name];
    fields.append(&mut dispatcher_args);
    Some(format!("{directive} = {}", fields.join(",")))
}

fn dispatcher_to_hyprland_args(name: &str, args: &[LuaValue]) -> Option<(String, Vec<String>)> {
    let name = name.strip_prefix("hl.dsp.").unwrap_or(name);
    let dispatcher = match name {
        "exec_cmd" => "exec",
        "exec_raw" => "execr",
        "exit" => "exit",
        "submap" => "submap",
        "pass" => "pass",
        "send_shortcut" => "sendshortcut",
        "send_key_state" => "sendkeystate",
        "layout" => "layoutmsg",
        "dpms" => "dpms",
        "event" => "event",
        "global" => "global",
        "force_renderer_reload" => "forcerendererreload",
        "force_idle" => "forceidle",
        "window.close" => "killactive",
        "window.kill" => "forcekillactive",
        "window.signal" => "signal",
        "window.float" | "window.toggle_floating" => "togglefloating",
        "window.fullscreen" | "window.toggle_fullscreen" => "fullscreen",
        "window.fullscreen_state" => "fullscreenstate",
        "window.pseudo" => "pseudo",
        "window.move" => "movewindow",
        "window.swap" => "swapwindow",
        "window.center" => "centerwindow",
        "window.cycle_next" => "cyclenext",
        "window.tag" => "tagwindow",
        "window.clear_tags" => "tagwindow",
        "window.toggle_swallow" => "toggleswallow",
        "window.pin" => "pin",
        "window.bring_to_top" => "bringactivetotop",
        "window.alter_zorder" => "alterzorder",
        "window.set_prop" => "setprop",
        "window.deny_from_group" => "denywindowfromgroup",
        "window.drag" => "dragwindow",
        "window.resize" => "resizeactive",
        "workspace.set" => "workspace",
        "workspace.toggle_special" => "togglespecialworkspace",
        "workspace.rename" => "renameworkspace",
        "workspace.move" => "movecurrentworkspacetomonitor",
        "workspace.swap_monitors" => "swapactiveworkspaces",
        "workspace.change_id" => "changegroupactive",
        "focus" => "movefocus",
        "no_op" => "nothing",
        "group.toggle" => "togglegroup",
        "group.next" => "changegroupactive",
        "group.prev" => "changegroupactive",
        "group.active" => "changegroupactive",
        "group.move_window" => "moveintogroup",
        "group.lock" => "lockgroups",
        "group.lock_active" => "lockactivegroup",
        "cursor.move_to_corner" => "movecursortocorner",
        "cursor.move" => "movecursor",
        _ => return None,
    };

    let mut values = Vec::new();
    for argument in args {
        match argument {
            LuaValue::Table(_) => {
                for key in [
                    "action",
                    "mode",
                    "internal",
                    "client",
                    "direction",
                    "workspace",
                    "monitor",
                    "monitor1",
                    "monitor2",
                    "signal",
                    "x",
                    "y",
                    "name",
                    "window",
                    "target",
                    "next",
                    "prev",
                    "into_group",
                    "into_or_create_group",
                    "out_of_group",
                    "group_aware",
                    "follow",
                    "index",
                    "forward",
                    "corner",
                    "mods",
                    "key",
                    "state",
                    "timeout",
                    "msg",
                    "message",
                    "prop",
                    "value",
                    "tag",
                    "mode",
                ] {
                    if let Some(value) = table_string(argument, key) {
                        values.push(value);
                    }
                }
            }
            other => {
                if let Some(value) = value_to_hyprlang(other) {
                    values.push(value);
                }
            }
        }
    }
    Some((dispatcher.to_string(), values))
}

fn window_rule_to_hyprlang(value: &LuaValue) -> Option<String> {
    let LuaValue::Table(entries) = value else {
        return None;
    };
    let mut effects = Vec::new();
    let mut matches = Vec::new();

    for (key, value) in entries {
        let Some(key) = key.as_deref() else {
            continue;
        };
        if key == "name" || key == "enabled" {
            continue;
        }
        if key == "match" {
            if let LuaValue::Table(properties) = value {
                for (property, value) in properties {
                    let Some(property) = property.as_deref() else {
                        continue;
                    };
                    let value = value_to_hyprlang(value)?;
                    matches.push(format!("{property}:{value}"));
                }
            }
        } else {
            let value = value_to_hyprlang(value)?;
            if value == "true" {
                effects.push(key.to_string());
            } else if value == "false" {
                effects.push(format!("no{key}"));
            } else {
                effects.push(format!("{key} {value}"));
            }
        }
    }

    if effects.is_empty() || matches.is_empty() {
        return None;
    }
    Some(format!(
        "windowrulev2 = {},{}",
        effects.join(" "),
        matches.join(",")
    ))
}

fn layer_rule_to_hyprlang(value: &LuaValue) -> Option<String> {
    let LuaValue::Table(entries) = value else {
        return None;
    };
    let mut namespace = None;
    let mut effects = Vec::new();
    for (key, value) in entries {
        let Some(key) = key.as_deref() else {
            continue;
        };
        if key == "match" {
            namespace = table_string(value, "namespace");
        } else if key != "name" && key != "enabled" {
            let value = value_to_hyprlang(value)?;
            if value == "true" {
                effects.push(key.to_string());
            } else if value == "false" {
                effects.push(format!("no{key}"));
            } else {
                effects.push(format!("{key} {value}"));
            }
        }
    }
    Some(format!("layerrule = {},{}", effects.join(" "), namespace?))
}

fn workspace_rule_to_hyprlang(value: &LuaValue) -> Option<String> {
    let workspace = table_string(value, "workspace")?;
    let mut fields = vec![workspace];
    for key in [
        "monitor",
        "default",
        "persistent",
        "gaps_in",
        "gaps_out",
        "float_gaps",
        "border_size",
        "no_border",
        "no_rounding",
        "decorate",
        "no_shadow",
        "on_created_empty",
        "default_name",
        "layout",
        "animation",
        "enabled",
    ] {
        if let Some(value) = table_string(value, key) {
            fields.push(format!("{key}:{value}"));
        }
    }
    Some(format!("workspace = {}", fields.join(", ")))
}

fn animation_to_hyprlang(value: &LuaValue) -> Option<String> {
    let leaf = table_string(value, "leaf")?;
    let enabled = table_string(value, "enabled").unwrap_or_else(|| "1".to_string());
    let speed = table_string(value, "speed").unwrap_or_else(|| "7".to_string());
    let curve = table_string(value, "bezier")
        .or_else(|| table_string(value, "spring").map(|name| format!("spring:{name}")))
        .unwrap_or_else(|| "default".to_string());
    let style = table_string(value, "style").unwrap_or_default();
    Some(format!(
        "animations {{\n    animation = {leaf},{enabled},{speed},{curve},{style}\n}}"
    ))
}

fn gesture_to_hyprlang(value: &LuaValue) -> Option<String> {
    let fingers = table_string(value, "fingers")?;
    let direction = table_string(value, "direction")?;
    let action = table_string(value, "action")?;
    Some(format!("gesture = {fingers},{direction},{action}"))
}

fn curve_to_hyprlang(name: &str, definition: &LuaValue) -> Option<String> {
    match table_string(definition, "type")?.as_str() {
        "bezier" => {
            let points = table_field(definition, "points").and_then(as_array)?;
            if points.len() != 2 {
                return None;
            }
            let first = as_array(&points[0])?;
            let second = as_array(&points[1])?;
            if first.len() != 2 || second.len() != 2 {
                return None;
            }
            let values = [
                value_to_hyprlang(&first[0])?,
                value_to_hyprlang(&first[1])?,
                value_to_hyprlang(&second[0])?,
                value_to_hyprlang(&second[1])?,
            ];
            Some(format!("bezier = {name},{}", values.join(",")))
        }
        "spring" => {
            let mass = table_string(definition, "mass")?;
            let stiffness = table_string(definition, "stiffness")?;
            let damping = table_string(definition, "damping")?;
            Some(format!("bezier = {name},spring,{mass},{stiffness},{damping}"))
        }
        _ => None,
    }
}

fn generic_section_to_hyprlang(
    section: &str,
    value: &LuaValue,
    required_key: &str,
) -> Option<String> {
    let LuaValue::Table(entries) = value else {
        return None;
    };
    let mut lines = Vec::new();
    for (key, value) in entries {
        let Some(key) = key.as_deref() else {
            continue;
        };
        if let Some(value) = value_to_hyprlang(value) {
            lines.push(format!("    {key} = {value}"));
        }
    }
    if !entries.iter().any(|(key, _)| key.as_deref() == Some(required_key)) {
        return None;
    }
    Some(format!("{section} {{\n{}\n}}", lines.join("\n")))
}

fn table_field<'a>(value: &'a LuaValue, key: &str) -> Option<&'a LuaValue> {
    let LuaValue::Table(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| (entry_key.as_deref() == Some(key)).then_some(value))
}

fn as_array(value: &LuaValue) -> Option<Vec<LuaValue>> {
    let LuaValue::Table(entries) = value else {
        return None;
    };
    if entries.iter().any(|(key, _)| key.is_some()) {
        return None;
    }
    Some(entries.iter().map(|(_, value)| value.clone()).collect())
}

fn table_string(table: &LuaValue, key: &str) -> Option<String> {
    table_field(table, key).and_then(value_to_hyprlang)
}

fn table_bool(table: &LuaValue, key: &str) -> Option<bool> {
    match table_string(table, key)?.as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn value_to_hyprlang(value: &LuaValue) -> Option<String> {
    match value {
        LuaValue::Scalar(value) => {
            let value = value.trim();
            if value == "nil" || value.starts_with("<Lua ") {
                None
            } else {
                Some(value.to_string())
            }
        }
        LuaValue::Table(entries) => {
            if let Some((_, colors)) = entries
                .iter()
                .find(|(key, _)| key.as_deref() == Some("colors"))
            {
                let colors = match colors {
                    LuaValue::Table(items) => items
                        .iter()
                        .filter_map(|(key, value)| {
                            if key.is_none() {
                                value_to_hyprlang(value)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => value_to_hyprlang(other).unwrap_or_default(),
                };
                let angle = entries.iter().find_map(|(key, value)| {
                    (key.as_deref() == Some("angle"))
                        .then(|| value_to_hyprlang(value))
                        .flatten()
                });
                return Some(match angle {
                    Some(angle) => format!("{colors} {angle}deg"),
                    None => colors,
                });
            }
            // Hyprland accepts CSS-gap and vec2 tables for some config values.
            // Convert those known value shapes instead of confusing them with sections.
            let keys = entries.iter()
                .filter_map(|(key, _)| key.as_deref())
                .collect::<Vec<_>>();
            if !keys.is_empty()
                && keys.iter().all(|key| ["top", "right", "bottom", "left"].contains(key))
            {
                let values = ["top", "right", "bottom", "left"]
                    .iter()
                    .filter_map(|key| table_string(value, key))
                    .collect::<Vec<_>>();
                return Some(values.join(", "));
            }
            if !keys.is_empty() && keys.iter().all(|key| ["x", "y"].contains(key)) {
                let values = ["x", "y"]
                    .iter()
                    .filter_map(|key| table_string(value, key))
                    .collect::<Vec<_>>();
                return Some(values.join(", "));
            }
            let values = entries
                .iter()
                .filter_map(|(key, value)| {
                    if key.is_some() {
                        None
                    } else {
                        value_to_hyprlang(value)
                    }
                })
                .collect::<Vec<_>>();
            if values.is_empty() {
                None
            } else {
                Some(values.join(", "))
            }
        }
    }
}

fn emit_config_table(table: &LuaValue, path: &mut Vec<String>, lines: &mut Vec<String>) {
    let LuaValue::Table(entries) = table else {
        return;
    };
    for (key, value) in entries {
        let Some(key) = key.as_deref() else {
            continue;
        };
        let mut full_path = path.clone();
        full_path.extend(key.split('.').map(str::to_string));
        match value {
            LuaValue::Table(children) if children.iter().any(|(key, _)| key.is_some()) => {
                emit_config_section(children, &full_path, lines);
            }
            other => {
                if let Some(value) = value_to_hyprlang(other)
                    && full_path.len() >= 2
                {
                    let category = full_path.remove(0);
                    let option = full_path.join(".");
                    lines.push(format!("{category} {{\n    {option} = {value}\n}}\n"));
                }
            }
        }
    }
}

fn emit_config_section(
    entries: &[(Option<String>, LuaValue)],
    path: &[String],
    lines: &mut Vec<String>,
) {
    if path.is_empty() {
        return;
    }
    let mut body = Vec::new();
    emit_section_body(entries, 1, &mut body);
    if !body.is_empty() {
        lines.push(format!("{} {{\n{}\n}}\n", path[0], body.join("\n")));
    }
}

fn emit_section_body(
    entries: &[(Option<String>, LuaValue)],
    depth: usize,
    lines: &mut Vec<String>,
) {
    for (key, value) in entries {
        let Some(key) = key.as_deref() else {
            continue;
        };
        match value {
            LuaValue::Table(children)
                if children.iter().any(|(key, _)| key.is_some())
                    && value_to_hyprlang(value).is_none() =>
            {
                lines.push(format!("{}{} {{", "    ".repeat(depth), key));
                emit_section_body(children, depth + 1, lines);
                lines.push(format!("{}}}", "    ".repeat(depth)));
            }
            other => {
                if let Some(value) = value_to_hyprlang(other) {
                    lines.push(format!("{}{} = {value}", "    ".repeat(depth), key));
                }
            }
        }
    }
}

fn lua_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

/// Store the complete Hyprlang serialization as a Lua string. This is
/// intentionally lossless: Hyprland permits repeated keys, positional fields,
/// and directives that the GUI does not have typed editors for.
#[derive(Default)]
struct ConfigNode {
    values: Vec<(String, String)>,
    children: BTreeMap<String, ConfigNode>,
}

impl ConfigNode {
    fn insert(&mut self, path: &[String], key: String, value: String) {
        if let Some((head, tail)) = path.split_first() {
            self.children
                .entry(head.clone())
                .or_default()
                .insert(tail, key, value);
        } else {
            self.values.push((key, value));
        }
    }
}

fn split_hyprlang_fields(value: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in value.chars() {
        if let Some(active_quote) = quote {
            current.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
        } else if character == '"' || character == '\'' {
            quote = Some(character);
            current.push(character);
        } else if character == ',' {
            fields.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(character);
        }
    }
    fields.push(current.trim().to_string());
    fields
}

fn unquote_hyprlang(value: &str) -> String {
    let value = value.trim();
    if value.len() < 2 {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    if !matches!((bytes[0], bytes[bytes.len() - 1]), (b'"', b'"') | (b'\'', b'\'')) {
        return value.to_string();
    }

    let mut output = String::new();
    let mut escaped = false;
    for character in value[1..value.len() - 1].chars() {
        if escaped {
            output.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

fn lua_key(key: &str) -> String {
    if !key.is_empty()
        && key.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric()
                    && (index > 0 || !character.is_ascii_digit())
        })
    {
        key.to_string()
    } else {
        format!("[{}]", lua_quote(key))
    }
}

fn lua_literal(value: &str) -> String {
    let value = unquote_hyprlang(value);
    if value == "true"
        || value == "false"
        || value == "nil"
        || value.parse::<i64>().is_ok()
        || value.parse::<f64>().is_ok()
    {
        value
    } else {
        lua_quote(&value)
    }
}

fn serialize_config_node(name: &str, node: &ConfigNode, depth: usize, output: &mut String) {
    let indent = "    ".repeat(depth);
    output.push_str(&format!("{indent}{} = {{\n", lua_key(name)));
    for (key, child) in &node.children {
        serialize_config_node(key, child, depth + 1, output);
    }
    for (key, value) in &node.values {
        output.push_str(&format!(
            "{}{} = {},\n",
            "    ".repeat(depth + 1),
            lua_key(key),
            lua_literal(value)
        ));
    }
    output.push_str(&format!("{indent}}},\n"));
}

fn lua_table_field(name: &str, value: &str) -> String {
    format!("{} = {}", lua_key(name), lua_literal(value))
}

fn serialize_monitor_directive(value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    let output = unquote_hyprlang(fields.first()?);
    if output.is_empty() {
        return None;
    }

    let mut lua_fields = vec![format!("output = {}", lua_quote(&output))];
    for (index, name) in [
        "mode",
        "position",
        "scale",
        "transform",
        "mirror",
        "bitdepth",
        "cm",
        "sdr_eotf",
        "sdrbrightness",
        "sdrsaturation",
        "vrr",
        "icc",
        "supports_wide_color",
        "supports_hdr",
        "sdr_min_luminance",
        "sdr_max_luminance",
        "min_luminance",
        "max_luminance",
        "max_avg_luminance",
        "reserved",
    ]
    .iter()
    .enumerate()
    {
        if let Some(field) = fields.get(index + 1).filter(|field| !field.is_empty()) {
            lua_fields.push(lua_table_field(name, field));
        }
    }
    if fields.iter().any(|field| field.trim() == "disabled") {
        lua_fields.push("disabled = true".to_string());
    }

    Some(format!("hl.monitor({{ {} }})\n", lua_fields.join(", ")))
}

fn serialize_animation_directive(value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    let leaf = unquote_hyprlang(fields.first()?);
    if leaf.is_empty() {
        return None;
    }
    let enabled = fields.get(1).map(|value| value == "1" || value == "true").unwrap_or(true);
    if !enabled {
        return Some(format!(
            "hl.animation({{ leaf = {}, enabled = false }})\n",
            lua_quote(&leaf)
        ));
    }
    let speed = fields.get(2).map(String::as_str).unwrap_or("7");
    let curve = fields.get(3).map(|value| unquote_hyprlang(value)).unwrap_or_else(|| "default".to_string());
    let style = fields.get(4).map(|value| unquote_hyprlang(value)).unwrap_or_default();

    let curve_field = if let Some(spring) = curve.strip_prefix("spring:") {
        format!("spring = {}", lua_quote(spring))
    } else {
        format!("bezier = {}", lua_quote(&curve))
    };
    let style_field = if style.is_empty() {
        String::new()
    } else {
        format!(", style = {}", lua_quote(&style))
    };
    Some(format!(
        "hl.animation({{ leaf = {}, enabled = true, speed = {}, {}{} }})\n",
        lua_quote(&leaf),
        lua_literal(speed),
        curve_field,
        style_field
    ))
}

fn serialize_bezier_directive(value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    let name = unquote_hyprlang(fields.first()?);
    if name.is_empty() {
        return None;
    }
    if fields.get(1).is_some_and(|field| field == "spring") {
        let mass = fields.get(2)?;
        let stiffness = fields.get(3)?;
        let damping = fields.get(4)?;
        return Some(format!(
            "hl.curve({}, {{ type = \"spring\", mass = {}, stiffness = {}, damping = {} }})\n",
            lua_quote(&name),
            lua_literal(mass),
            lua_literal(stiffness),
            lua_literal(damping)
        ));
    }
    if fields.len() < 5 {
        return None;
    }
    Some(format!(
        "hl.curve({}, {{ type = \"bezier\", points = {{ {{ {}, {} }}, {{ {}, {} }} }} }})\n",
        lua_quote(&name),
        lua_literal(&fields[1]),
        lua_literal(&fields[2]),
        lua_literal(&fields[3]),
        lua_literal(&fields[4])
    ))
}

fn serialize_bind_directive(key: &str, value: &str) -> Option<String> {
    let flags = key.strip_prefix("bind")?;
    if !flags.chars().all(|flag| "lermns".contains(flag)) {
        return None;
    }
    let parts = split_hyprlang_fields(value);
    if parts.len() < 3 {
        return None;
    }
    let modifiers = parts[0]
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let key_name = unquote_hyprlang(&parts[1]);
    if key_name.is_empty() {
        return None;
    }
    let chord = if modifiers.is_empty() {
        key_name
    } else {
        format!("{} + {key_name}", modifiers.join(" + "))
    };
    let dispatcher = parts[2].as_str();
    let args = parts.get(3..).unwrap_or_default().join(",");
    let argument = unquote_hyprlang(&args);
    let call = match dispatcher {
        "exec" => format!("hl.dsp.exec_cmd({})", lua_quote(&argument)),
        "execr" => format!("hl.dsp.exec_raw({})", lua_quote(&argument)),
        "exit" => "hl.dsp.exit()".to_string(),
        "submap" => format!("hl.dsp.submap({})", lua_quote(&argument)),
        "pass" => format!("hl.dsp.pass({})", lua_quote(&argument)),
        "sendshortcut" => format!("hl.dsp.send_shortcut({})", lua_quote(&argument)),
        "sendkeystate" => format!("hl.dsp.send_key_state({})", lua_quote(&argument)),
        "layoutmsg" => format!("hl.dsp.layout({})", lua_quote(&argument)),
        "dpms" => format!("hl.dsp.dpms({})", lua_quote(&argument)),
        "event" => format!("hl.dsp.event({})", lua_quote(&argument)),
        "global" => format!("hl.dsp.global({})", lua_quote(&argument)),
        "forcerendererreload" => "hl.dsp.force_renderer_reload()".to_string(),
        "forceidle" => format!("hl.dsp.force_idle({})", lua_quote(&argument)),
        "killactive" => "hl.dsp.window.close()".to_string(),
        "forcekillactive" => "hl.dsp.window.kill()".to_string(),
        "togglefloating" => "hl.dsp.window.float({ action = \"toggle\" })".to_string(),
        "fullscreen" => "hl.dsp.window.fullscreen({ action = \"toggle\" })".to_string(),
        "pseudo" => "hl.dsp.window.pseudo()".to_string(),
        "movewindow" => format!("hl.dsp.window.move({{ workspace = {} }})", lua_quote(&argument)),
        "swapwindow" => format!("hl.dsp.window.swap({{ direction = {} }})", lua_quote(&argument)),
        "centerwindow" => "hl.dsp.window.center()".to_string(),
        "cyclenext" => "hl.dsp.window.cycle_next()".to_string(),
        "tagwindow" => format!("hl.dsp.window.tag({})", lua_quote(&argument)),
        "toggleswallow" => "hl.dsp.window.toggle_swallow()".to_string(),
        "pin" => "hl.dsp.window.pin()".to_string(),
        "bringactivetotop" => "hl.dsp.window.bring_to_top()".to_string(),
        "alterzorder" => format!("hl.dsp.window.alter_zorder({})", lua_quote(&argument)),
        "setprop" => format!("hl.dsp.window.set_prop({})", lua_quote(&argument)),
        "denywindowfromgroup" => "hl.dsp.window.deny_from_group()".to_string(),
        "dragwindow" => "hl.dsp.window.drag()".to_string(),
        "resizeactive" => "hl.dsp.window.resize()".to_string(),
        "workspace" => format!("hl.dsp.focus({{ workspace = {} }})", lua_quote(&argument)),
        "movetoworkspace" | "movetoworkspacesilent" => format!("hl.dsp.window.move({{ workspace = {} }})", lua_quote(&argument)),
        "togglespecialworkspace" => format!("hl.dsp.workspace.toggle_special({})", lua_quote(&argument)),
        "renameworkspace" => format!("hl.dsp.workspace.rename({{ workspace = {} }})", lua_quote(&argument)),
        "movecurrentworkspacetomonitor" => format!("hl.dsp.workspace.move({{ monitor = {} }})", lua_quote(&argument)),
        "swapactiveworkspaces" => format!("hl.dsp.workspace.swap_monitors({{ monitor1 = {} }})", lua_quote(&argument)),
        "movefocus" => format!("hl.dsp.focus({{ direction = {} }})", lua_quote(&argument)),
        "focusmonitor" => format!("hl.dsp.focus({{ monitor = {} }})", lua_quote(&argument)),
        "nothing" => "hl.dsp.no_op()".to_string(),
        _ => return None,
    };

    let mut options = Vec::new();
    if flags.contains('l') { options.push("locked = true"); }
    if flags.contains('e') { options.push("repeating = true"); }
    if flags.contains('r') { options.push("release = true"); }
    if flags.contains('m') { options.push("mouse = true"); }
    if flags.contains('n') { options.push("non_consuming = true"); }
    if flags.contains('s') { options.push("submap_universal = true"); }
    let options = if options.is_empty() {
        String::new()
    } else {
        format!(", {{ {} }}", options.join(", "))
    };
    Some(format!(
        "hl.bind({}, {}{})\n",
        lua_quote(&chord),
        call,
        options
    ))
}

fn serialize_workspace_directive(value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    let workspace = unquote_hyprlang(fields.first()?);
    if workspace.is_empty() {
        return None;
    }
    let mut table = vec![format!("workspace = {}", lua_quote(&workspace))];
    for field in fields.iter().skip(1) {
        if let Some((key, value)) = field.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() && !value.is_empty() {
                table.push(lua_table_field(key, value));
            }
        }
    }
    Some(format!("hl.workspace_rule({{ {} }})\n", table.join(", ")))
}

fn serialize_windowrule_directive(value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    let effects = fields.first()?.split_whitespace().collect::<Vec<_>>();
    let match_fields = fields.iter().skip(1).filter_map(|field| {
        let (key, value) = field.split_once(':')?;
        Some(format!("{} = {}", lua_key(key.trim()), lua_quote(&unquote_hyprlang(value))))
    }).collect::<Vec<_>>();
    if match_fields.is_empty() {
        return None;
    }

    let mut table = vec![format!("match = {{ {} }}", match_fields.join(", "))];
    for effect in effects {
        match effect {
            "float" | "tile" | "fullscreen" | "pin" | "center" | "no_focus"
            | "no_blur" | "no_shadow" | "no_anim" | "no_initial_focus" | "no_close"
            | "immediate" | "opaque" | "force_rgbx" | "sync_fullscreen"
            | "no_vrr" | "stay_focused" => table.push(format!("{effect} = true")),
            "noblur" => table.push("blur = false".to_string()),
            "noborder" => table.push("border_size = 0".to_string()),
            "nomaxsize" => table.push("max_size = false".to_string()),
            _ => {}
        }
    }
    Some(format!("hl.window_rule({{ {} }})\n", table.join(", ")))
}

fn serialize_layer_rule_directive(value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    let effects = fields.first()?.split_whitespace().collect::<Vec<_>>();
    let namespace = fields.get(1).map(|value| unquote_hyprlang(value))?;
    if namespace.is_empty() {
        return None;
    }
    let mut table = vec![format!("match = {{ namespace = {} }}", lua_quote(&namespace))];
    for effect in effects {
        match effect {
            "blur" | "blur_popups" | "ignore_alpha" | "dim_around" | "xray"
            | "no_anim" | "no_screen_share" => {
                table.push(format!("{effect} = true"));
            }
            _ => {}
        }
    }
    Some(format!("hl.layer_rule({{ {} }})\n", table.join(", ")))
}

fn serialize_directive(key: &str, value: &str) -> Option<String> {
    let fields = split_hyprlang_fields(value);
    match key {
        "monitor" => serialize_monitor_directive(value),
        "env" => {
            let name = unquote_hyprlang(fields.first()?);
            let val = unquote_hyprlang(fields.get(1)?);
            let dbus = fields.get(2).map(|value| value == "true" || value == "1").unwrap_or(false);
            Some(format!(
                "hl.env({}, {}{})\n",
                lua_quote(&name),
                lua_quote(&val),
                if dbus { ", true" } else { "" }
            ))
        }
        "exec" => Some(format!(
            "hl.exec_cmd({})\n",
            lua_quote(&unquote_hyprlang(value))
        )),
        "exec-once" => Some(format!(
            "hl.on(\"hyprland.start\", function() hl.exec_cmd({}) end)\n",
            lua_quote(&unquote_hyprlang(value))
        )),
        "bezier" => serialize_bezier_directive(value),
        "animation" => serialize_animation_directive(value),
        "workspace" => serialize_workspace_directive(value),
        "windowrule" | "windowrulev2" => serialize_windowrule_directive(value),
        "layerrule" => serialize_layer_rule_directive(value),
        "gesture" => {
            if fields.len() < 3 { return None; }
            Some(format!(
                "hl.gesture({{ fingers = {}, direction = {}, action = {} }})\n",
                lua_literal(&fields[0]),
                lua_quote(&unquote_hyprlang(&fields[1])),
                lua_quote(&unquote_hyprlang(&fields[2]))
            ))
        }
        "permission" => {
            if fields.len() < 3 { return None; }
            Some(format!(
                "hl.permission({}, {}, {})\n",
                lua_quote(&unquote_hyprlang(&fields[0])),
                lua_quote(&unquote_hyprlang(&fields[1])),
                lua_quote(&unquote_hyprlang(&fields[2]))
            ))
        }
        "unbind" => {
            let key = fields.iter().map(|part| unquote_hyprlang(part)).collect::<Vec<_>>().join("+");
            Some(format!("hl.unbind({})\n", lua_quote(&key)))
        }
        key if key.starts_with("bind") => serialize_bind_directive(key, value),
        _ => None,
    }
}

pub fn hyprlang_to_lua(source: &str) -> String {
    let mut root = ConfigNode::default();
    let mut directives = Vec::<(String, String)>::new();
    let mut devices = Vec::<Vec<(String, String)>>::new();
    let mut current_device = None::<Vec<(String, String)>>;
    let mut section_stack = Vec::<String>::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "}" {
            if let Some(section) = section_stack.pop()
                && section == "device"
                && section_stack.is_empty()
            {
                if let Some(device) = current_device.take() {
                    devices.push(device);
                }
            }
            continue;
        }
        if let Some(section) = trimmed.strip_suffix('{') {
            let section = section.trim().to_string();
            if section_stack.is_empty() && section == "device" {
                current_device = Some(Vec::new());
            }
            section_stack.push(section);
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = strip_unquoted_comment(value).trim().to_string();

        if section_stack.len() == 1 && section_stack[0] == "animations"
            && (key == "animation" || key == "bezier")
        {
            directives.push((key, value));
        } else if section_stack.len() == 1 && section_stack[0] == "device" {
            if let Some(device) = current_device.as_mut() {
                device.push((key, value));
            }
        } else if section_stack.is_empty() {
            directives.push((key, value));
        } else {
            root.insert(&section_stack, key, value);
        }
    }

    let mut output = String::from(
        "-- BEGIN HYPRVIZ MANAGED CONFIG\n-- Generated by HyprViz. Changes are managed by the GUI.\n",
    );
    if !root.children.is_empty() || !root.values.is_empty() {
        output.push_str("hl.config({\n");
        for (key, child) in &root.children {
            serialize_config_node(key, child, 1, &mut output);
        }
        for (key, value) in &root.values {
            output.push_str(&format!("    {} = {},\n", lua_key(key), lua_literal(value)));
        }
        output.push_str("})\n");
    }
    for device in devices {
        let fields = device.iter()
            .map(|(key, value)| lua_table_field(key, value))
            .collect::<Vec<_>>();
        if fields.iter().any(|field| field.starts_with("name = ")) {
            output.push_str(&format!("hl.device({{ {} }})\n", fields.join(", ")));
        }
    }
    for (key, value) in directives {
        if let Some(line) = serialize_directive(&key, &value) {
            output.push_str(&line);
        } else {
            output.push_str(&format!(
                "-- Legacy directive needs a Lua API mapping: {}\n",
                lua_quote(&format!("{key} = {value}"))
            ));
        }
    }
    output.push_str("-- END HYPRVIZ MANAGED CONFIG\n");
    output
}

pub fn merge_managed_lua(existing: &str, generated: &str) -> String {
    const START: &str = "-- BEGIN HYPRVIZ MANAGED CONFIG";
    const END: &str = "-- END HYPRVIZ MANAGED CONFIG";
    if let Some(start) = existing.find(START)
        && let Some(relative_end) = existing[start..].find(END)
    {
        let end = start + relative_end + END.len();
        let mut merged = String::with_capacity(existing.len() + generated.len());
        merged.push_str(&existing[..start]);
        merged.push_str(generated.trim());
        merged.push_str(&existing[end..]);
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        return merged;
    }

    let mut merged = existing.trim_end().to_string();
    if !merged.is_empty() {
        merged.push_str("\n\n");
    }
    merged.push_str(generated.trim());
    merged.push('\n');
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hyprviz-lua-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn preserves_repeated_directives_inside_sections() {
        let source = concat!(
            "animations {\n",
            "    animation = windows, 1, 4, default\n",
            "    animation = workspaces, 1, 5, default\n",
            "}\n",
        );
        let generated = hyprlang_to_lua(source);
        let parsed = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some(&generated),
            false,
        )
        .unwrap();
        assert_eq!(parsed.matches("animation =").count(), 2, "{parsed}");
    }

    #[test]
    fn preserves_all_positional_monitor_fields() {
        let source = concat!(
            "monitor = eDP-1, preferred, auto, 1, 1, HDMI-A-1, 10, srgb\n",
        );
        let generated = hyprlang_to_lua(source);
        let parsed =
            expand_lua_config(Path::new("/tmp/hyprland.lua"), Some(&generated), false).unwrap();
        assert!(
            parsed.contains("monitor = eDP-1, preferred, auto, 1, 1, HDMI-A-1, 10, srgb"),
            "{parsed}"
        );
    }

    #[test]
    fn preserves_supported_bind_flags_when_round_tripping() {
        let source = concat!(
            "bindm = , mouse:272, exec, notify-send mouse\n",
            "bindn = , Q, exec, notify-send non-consuming\n",
            "binds = SUPER, Q, exec, notify-send universal\n",
        );
        let generated = hyprlang_to_lua(source);
        let parsed =
            expand_lua_config(Path::new("/tmp/hyprland.lua"), Some(&generated), false).unwrap();
        assert!(parsed.contains("bindm = ,mouse:272,exec,notify-send mouse"), "{parsed}");
        assert!(parsed.contains("bindn = ,Q,exec,notify-send non-consuming"), "{parsed}");
        assert!(parsed.contains("binds = SUPER,Q,exec,notify-send universal"), "{parsed}");
    }

    #[test]
    fn preserves_hashes_and_commas_inside_quoted_directive_values() {
        let generated =
            hyprlang_to_lua("env = GTK_THEME,\"Adwaita # dark, compact\"\n");
        let parsed = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some(&generated),
            false,
        )
        .unwrap();
        assert!(
            parsed.contains("env = GTK_THEME,\"Adwaita # dark, compact\""),
            "{parsed}"
        );
    }

    #[test]
    fn preserves_unknown_hyprland_directives_through_lua_round_trip() {
        let generated = hyprlang_to_lua("exec-once = notify-send\nunknown-setting = keep-me\n");
        assert!(!generated.contains("hl.raw("));
        assert!(generated.contains("hl.on(\"hyprland.start\""));
        assert!(generated.contains("Legacy directive needs a Lua API mapping"));
        let parsed =
            expand_lua_config(Path::new("/tmp/hyprland.lua"), Some(&generated), false).unwrap();
        assert!(parsed.contains("exec-once = notify-send"), "{parsed}");
        assert!(!parsed.contains("unknown-setting = keep-me"), "{parsed}");
    }

    #[test]
    fn collects_startup_commands_and_serializes_unbind_keys() {
        let parsed = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some(
                r#"
                hl.on("hyprland.start", function()
                    hl.exec_cmd("notify-send boot")
                end)
                hl.exec_cmd("notify-send manual")
                hl.unbind("SUPER+Q")
                "#,
            ),
            false,
        )
        .unwrap();
        assert!(parsed.contains("exec-once = notify-send boot"), "{parsed}");
        assert!(parsed.contains("exec = notify-send manual"), "{parsed}");
        assert!(parsed.contains("unbind = SUPER,Q"), "{parsed}");
    }

    #[test]
    fn executes_real_lua_semantics_and_local_require() {
        let dir = test_dir("runtime");
        fs::write(
            dir.join("values.lua"),
            "local start = 4\nreturn { base = start + 3 }\n",
        )
        .unwrap();
        let config = r#"
            local values = require("./values")
            local base = values.base
            local function compute()
                local total = 0
                for i = 1, base do
                    total = total + i
                end
                return total
            end
            if base == 7 then
                hl.config({ general = { gaps_in = compute(), gaps_out = base } })
            end
        "#;
        let parsed = expand_lua_config(&dir.join("hyprland.lua"), Some(config), false).unwrap();
        assert!(parsed.contains("gaps_in = 28"), "{parsed}");
        assert!(parsed.contains("gaps_out = 7"), "{parsed}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn evaluates_config_tables_and_get_config_as_lua_values() {
        let parsed = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some(
                r#"
                local gaps = { top = 3, right = 4, bottom = 5, left = 6 }
                hl.config({ general = { gaps_in = gaps } })
                local current = hl.get_config("general.gaps_in")
                hl.config({ general = { gaps_out = current.top + current.right } })
                "#,
            ),
            false,
        )
        .unwrap();
        assert!(parsed.contains("gaps_in = 3, 4, 5, 6"), "{parsed}");
        assert!(parsed.contains("gaps_out = 7"), "{parsed}");
    }

    #[test]
    fn loadfile_and_dofile_are_relative_to_the_calling_module() {
        let dir = test_dir("loadfile");
        fs::write(
            dir.join("nested.lua"),
            "local values = require('values')\nreturn values.base, values.base + 1\n",
        )
        .unwrap();
        fs::write(dir.join("values.lua"), "return { base = 9 }\n").unwrap();
        let source = r#"
            local chunk = assert(loadfile("./nested.lua"))
            local first, second = chunk()
            local third, fourth = dofile("./nested.lua")
            hl.config({ general = { gaps_in = first + third, gaps_out = second + fourth } })
        "#;
        let parsed = expand_lua_config(&dir.join("hyprland.lua"), Some(source), false).unwrap();
        assert!(parsed.contains("gaps_in = 18"), "{parsed}");
        assert!(parsed.contains("gaps_out = 20"), "{parsed}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn evaluates_bind_dispatcher_closures_and_flags() {
        let parsed = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some(
                r#"
                local main = "SUPER"
                local key = main .. "+Q"
                hl.bind(key, hl.dsp.exec_cmd("kitty"), { locked = true, repeating = true })
                hl.bind("SUPER+X", hl.dsp.window.close())
                "#,
            ),
            false,
        )
        .unwrap();
        assert!(parsed.contains("bindle = SUPER,Q,exec,kitty"), "{parsed}");
        assert!(parsed.contains("bind = SUPER,X,killactive"), "{parsed}");
    }

    #[test]
    fn excludes_managed_module_without_executing_it() {
        let dir = test_dir("exclude");
        fs::write(
            dir.join("hyprviz.lua"),
            "error('managed module should not be executed')",
        )
        .unwrap();
        let parsed = expand_lua_config(
            &dir.join("hyprland.lua"),
            Some("require('./hyprviz')\nhl.config({ general = { gaps_in = 5 } })"),
            true,
        )
        .unwrap();
        assert!(parsed.contains("gaps_in = 5"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn blocks_process_and_general_file_io_but_keeps_environment_helpers() {
        let error = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some("os.execute('touch /tmp/hyprviz-should-not-exist')"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("attempt to call a nil value"), "{error}");

        let error = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some("io.open('/tmp/hyprviz-should-not-exist', 'w')"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("attempt to index a nil value"), "{error}");

        let error = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some("os.remove('/tmp/hyprviz-should-not-exist')"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("attempt to call a nil value"), "{error}");

        assert!(expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some("local home = os.getenv('HOME') or ''\\nlocal os_module = require('os')\\nassert(os_module.execute == nil and os_module.remove == nil)\\nhl.config({ general = { gaps_in = #home } })"),
            false,
        ).is_ok());
    }

    #[test]
    fn reports_lua_runtime_errors() {
        let error = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some("local = this is not valid Lua"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("syntax") || error.to_string().contains("unexpected"));
    }

    #[test]
    fn caps_lua_memory_allocations() {
        let error = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some(
                "local values = {}\nwhile true do values[#values + 1] = string.rep('x', 4096) end",
            ),
            false,
        )
        .unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("memory")
                || error.to_string().contains("instruction evaluation limit"),
            "{error}"
        );
    }

    #[test]
    fn rejects_infinite_lua_loops() {
        let error = expand_lua_config(
            Path::new("/tmp/hyprland.lua"),
            Some("while true do end"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("instruction evaluation limit"));
    }

    #[test]
    fn detects_only_standalone_managed_require_statements() {
        assert!(is_hyprviz_require_line("require('./hyprviz')"));
        assert!(is_hyprviz_require_line("  require('./hyprviz/profile'); -- managed"));
        assert!(!is_hyprviz_require_line("require('./hyprviz'); print('keep this')"));
        assert!(!is_hyprviz_require_line("local config = require('./hyprviz')"));
        assert!(!is_hyprviz_require_line("object.require('./hyprviz')"));
    }

    #[test]
    fn ignores_require_text_inside_long_comments_and_strings() {
        let source = r#"
            --[=[ require("./hyprviz/commented") ]=]
            local text = [==[
                require("./hyprviz/string")
            ]==]
            require("./hyprviz/active")
        "#;
        assert_eq!(
            profile_name_from_lua(source),
            Some("active".to_string())
        );
    }

    #[test]
    fn detects_and_switches_profile_requires() {
        assert_eq!(
            profile_name_from_lua("require('./hyprviz/custom')"),
            Some("custom".to_string())
        );
        assert!(has_hyprviz_require("require('./hyprviz')"));
        assert_eq!(
            profile_name_from_lua("require('./hyprviz')"),
            Some("Default".to_string())
        );
    }

    #[test]
    fn serializes_and_merges_a_managed_lua_block() {
        let generated = hyprlang_to_lua("general {\n    gaps_in = 5\n}\n");
        assert!(generated.contains("hl.config({"));
        assert!(generated.contains("gaps_in = 5"));
        let merged = merge_managed_lua(
            "local custom = true\n-- BEGIN HYPRVIZ MANAGED CONFIG\nold()\n-- END HYPRVIZ MANAGED CONFIG\nprint(custom)\n",
            &generated,
        );
        assert!(merged.contains("local custom = true"));
        assert!(merged.contains("print(custom)"));
        assert!(merged.contains("gaps_in = 5"));
        assert!(!merged.contains("old()"));
    }
}
