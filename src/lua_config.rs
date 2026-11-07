use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
enum LuaValue {
    Scalar(String),
    Table(Vec<(Option<String>, LuaValue)>),
}

#[derive(Debug)]
struct LuaCall {
    name: String,
    args: String,
}

pub fn is_lua_config_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("lua")
}

pub fn has_hyprviz_require(source: &str) -> bool {
    calls(source).iter().any(|call| {
        call.name == "require"
            && call.args.split(',').next().and_then(parse_string_literal)
                .is_some_and(|module| is_hyprviz_module(&module))
    })
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
    let mut active = None;
    for call in calls(source) {
        if call.name != "require" { continue; }
        let Some(module) = call.args.split(',').next().and_then(parse_string_literal) else { continue; };
        if module == "./hyprviz" || module == "hyprviz" {
            active = Some("Default".to_string());
        } else if let Some(name) = module.strip_prefix("./hyprviz/").or_else(|| module.strip_prefix("hyprviz/")) {
            let name = name.trim_end_matches(".lua");
            if !name.is_empty() && !name.contains('/') { active = Some(name.to_string()); }
        }
    }
    active
}

pub fn expand_lua_config(entry_path: &Path, entry: Option<&str>, exclude_hyprviz: bool) -> Result<String, Box<dyn Error>> {
    let config_dir = entry_path.parent().unwrap_or_else(|| Path::new("."));
    let mut visited = HashSet::new();
    let mut output = String::new();
    process_lua_source(entry_path, entry, config_dir, exclude_hyprviz, &mut visited, &mut output)?;
    Ok(output)
}

fn process_lua_source(
    path: &Path,
    supplied_source: Option<&str>,
    config_dir: &Path,
    exclude_hyprviz: bool,
    visited: &mut HashSet<PathBuf>,
    output: &mut String,
) -> Result<(), Box<dyn Error>> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) { return Ok(()); }
    let owned;
    let source = if let Some(source) = supplied_source { source } else {
        owned = fs::read_to_string(path)?;
        &owned
    };

    for call in calls(source) {
        if call.name == "require" {
            let Some(module) = call.args.split(',').next().and_then(parse_string_literal) else { continue; };
            if exclude_hyprviz && is_hyprviz_module(&module) { continue; }
            for module_path in resolve_required_modules(&module, path, config_dir) {
                process_lua_source(&module_path, None, config_dir, exclude_hyprviz, visited, output)?;
            }
            continue;
        }
        if let Some(converted) = call_to_hyprlang(&call) {
            output.push_str(&converted);
            if !converted.ends_with('\n') { output.push('\n'); }
        }
    }
    Ok(())
}

fn is_hyprviz_module(module: &str) -> bool {
    module == "./hyprviz" || module == "hyprviz"
        || module.starts_with("./hyprviz/") || module.starts_with("hyprviz/")
}

fn resolve_required_modules(module: &str, source_path: &Path, config_dir: &Path) -> Vec<PathBuf> {
    let base_dir = if module.starts_with("./") || module.starts_with("../") {
        source_path.parent().unwrap_or(config_dir)
    } else { config_dir };

    let relative = if module.starts_with('/') || module.starts_with("./") || module.starts_with("../")
        || module.contains('/') || module.ends_with(".lua") {
        PathBuf::from(module)
    } else { PathBuf::from(module.replace('.', "/")) };

    if module.contains('*') {
        let pattern_path = base_dir.join(&relative);
        let Some(pattern) = pattern_path.file_name().and_then(|s| s.to_str()) else { return Vec::new(); };
        let Some(parent) = pattern_path.parent() else { return Vec::new(); };
        let (prefix, suffix) = pattern.split_once('*').unwrap_or((pattern, ""));
        let Ok(entries) = fs::read_dir(parent) else { return Vec::new(); };
        let mut matches = entries.flatten().map(|entry| entry.path()).filter(|candidate| {
            candidate.extension().and_then(|s| s.to_str()) == Some("lua")
                && candidate.file_name().and_then(|s| s.to_str())
                    .is_some_and(|name| name.starts_with(prefix) && name.ends_with(suffix))
        }).collect::<Vec<_>>();
        matches.sort();
        return matches;
    }

    let mut candidate = if relative.is_absolute() { relative } else { base_dir.join(relative) };
    if candidate.extension().is_none() { candidate.set_extension("lua"); }
    if candidate.is_file() { vec![candidate] } else { Vec::new() }
}

pub fn lua_to_hyprlang(source: &str) -> String {
    let mut output = String::new();
    for call in calls(source) {
        if call.name == "require" { continue; }
        if let Some(converted) = call_to_hyprlang(&call) {
            output.push_str(&converted);
            if !converted.ends_with('\n') { output.push('\n'); }
        }
    }
    output
}

fn call_to_hyprlang(call: &LuaCall) -> Option<String> {
    let args = split_top_level(&call.args, ',');
    match call.name.as_str() {
        "hl.config" => {
            let table = parse_table_arg(args.first()?)?;
            let mut lines = Vec::new();
            emit_config_table(&table, &mut Vec::new(), &mut lines);
            Some(lines.join("\n") + if lines.is_empty() { "" } else { "\n" })
        }
        "hl.monitor" => {
            let table = parse_table_arg(args.first()?)?;
            let output = table_string(&table, "output")?;
            let mode = table_string(&table, "mode").unwrap_or_else(|| "preferred".to_string());
            let position = table_string(&table, "position").unwrap_or_else(|| "0x0".to_string());
            let scale = table_string(&table, "scale").unwrap_or_else(|| "1".to_string());
            Some(format!("monitor = {output},{mode},{position},{scale}\n"))
        }
        "hl.env" => {
            let name = parse_string_literal(args.first()?)?;
            let value = args.get(1).and_then(|arg| parse_string_literal(arg)).unwrap_or_default();
            Some(format!("env = {name},{value}\n"))
        }
        "hl.bind" => lua_bind_to_hyprlang(&args),
        "hl.window_rule" => {
            let table = parse_table_arg(args.first()?)?;
            window_rule_to_hyprlang(&table)
        }
        "hl.animation" => {
            let table = parse_table_arg(args.first()?)?;
            let leaf = table_string(&table, "leaf")?;
            let enabled = table_string(&table, "enabled").unwrap_or_else(|| "1".to_string());
            let speed = table_string(&table, "speed").unwrap_or_else(|| "7".to_string());
            let curve = table_string(&table, "bezier").or_else(|| table_string(&table, "spring"))
                .unwrap_or_else(|| "default".to_string());
            let style = table_string(&table, "style").unwrap_or_default();
            Some(format!("animations {{\n    animation = {leaf},{enabled},{speed},{curve},{style}\n}}\n"))
        }
        "hl.gesture" => {
            let table = parse_table_arg(args.first()?)?;
            Some(format!(
                "gesture = {},{},{}\n",
                table_string(&table, "fingers")?,
                table_string(&table, "direction")?,
                table_string(&table, "action")?
            ))
        }
        _ => None,
    }
}

fn lua_bind_to_hyprlang(args: &[String]) -> Option<String> {
    let key = parse_string_literal(args.first()?)?;
    let dispatcher_call = args.get(1)?.trim();
    let start = dispatcher_call.find("hl.dsp.")? + "hl.dsp.".len();
    let rest = &dispatcher_call[start..];
    let open = rest.find('(')?;
    let method = rest[..open].trim();
    let close = find_matching(rest, open, '(', ')')?;
    let dispatcher_args = split_top_level(&rest[open + 1..close], ',');
    let dispatcher = match method {
        "exec_cmd" => "exec",
        "window.close" => "killactive",
        "window.toggle_floating" => "togglefloating",
        "window.toggle_fullscreen" => "fullscreen",
        "exit" => "exit",
        "workspace.set" => "workspace",
        "window.move_to_workspace" => "movetoworkspace",
        _ => return None,
    };
    let key_parts = key.split('+').map(str::trim).collect::<Vec<_>>();
    let key_name = key_parts.last()?.to_string();
    let modifiers = key_parts[..key_parts.len() - 1].join("_");
    let mut value = format!("{},{}", modifiers, key_name);
    value.push(',');
    value.push_str(dispatcher);
    if let Some(first) = dispatcher_args.first() {
        let arg = parse_string_literal(first).unwrap_or_else(|| first.trim().to_string());
        value.push(',');
        value.push_str(&arg);
    }
    let flags = args.get(2).and_then(|arg| parse_table_arg(arg));
    let mut directive = "bind".to_string();
    if let Some(flags) = flags {
        if table_bool(&flags, "locked") == Some(true) { directive.push('l'); }
        if table_bool(&flags, "repeat") == Some(true) || table_bool(&flags, "repeating") == Some(true) {
            directive.push('e');
        }
        if table_bool(&flags, "mouse") == Some(true) { directive.push('m'); }
    }
    Some(format!("{directive} = {value}\n"))
}

fn window_rule_to_hyprlang(table: &LuaValue) -> Option<String> {
    let LuaValue::Table(rule) = table else { return None; };
    let mut effects = Vec::new();
    let mut matches = Vec::new();
    for (key, value) in rule {
        let Some(key) = key.as_deref() else { continue; };
        if key == "name" { continue; }
        if key == "match" {
            if let LuaValue::Table(properties) = value {
                for (property, value) in properties {
                    let Some(property) = property.as_deref() else { continue; };
                    matches.push(format!("{property}:{}", value_to_hyprlang(value)?));
                }
            }
        } else {
            let value = value_to_hyprlang(value)?;
            if value == "true" { effects.push(key.to_string()); }
            else if value == "false" { effects.push(format!("no{key}")); }
            else { effects.push(format!("{key} {value}")); }
        }
    }
    if effects.is_empty() || matches.is_empty() { return None; }
    Some(format!("windowrulev2 = {},{}\n", effects.join(" "), matches.join(",")))
}

fn emit_config_table(table: &LuaValue, path: &mut Vec<String>, lines: &mut Vec<String>) {
    let LuaValue::Table(entries) = table else { return; };
    for (key, value) in entries {
        let Some(key) = key.as_deref() else { continue; };
        let mut full_path = path.clone();
        full_path.extend(key.split('.').map(str::to_string));
        match value {
            LuaValue::Table(children) if children.iter().any(|(key, _)| key.is_some()) => {
                emit_config_section(children, &full_path, lines);
            }
            other => {
                if let Some(value) = value_to_hyprlang(other) && full_path.len() >= 2 {
                    let category = full_path.remove(0);
                    let option = full_path.join(".");
                    lines.push(format!("{category} {{\n    {option} = {value}\n}}\n"));
                }
            }
        }
    }
}

fn emit_config_section(entries: &[(Option<String>, LuaValue)], path: &[String], lines: &mut Vec<String>) {
    if path.is_empty() { return; }
    let mut body = Vec::new();
    emit_section_body(entries, 1, &mut body);
    if !body.is_empty() {
        lines.push(format!("{} {{\n{}\n}}\n", path[0], body.join("\n")));
    }
}

fn emit_section_body(entries: &[(Option<String>, LuaValue)], depth: usize, lines: &mut Vec<String>) {
    for (key, value) in entries {
        let Some(key) = key.as_deref() else { continue; };
        match value {
            LuaValue::Table(children) if children.iter().any(|(key, _)| key.is_some()) => {
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

fn value_to_hyprlang(value: &LuaValue) -> Option<String> {
    match value {
        LuaValue::Scalar(value) => {
            let value = value.trim();
            if value == "nil" { None } else { Some(value.to_string()) }
        }
        LuaValue::Table(entries) => {
            if let Some((_, colors)) = entries.iter().find(|(key, _)| key.as_deref() == Some("colors")) {
                let colors = match colors {
                    LuaValue::Table(items) => items.iter()
                        .filter_map(|(key, value)| if key.is_none() { value_to_hyprlang(value) } else { None })
                        .collect::<Vec<_>>().join(" "),
                    other => value_to_hyprlang(other).unwrap_or_default(),
                };
                let angle = entries.iter().find_map(|(key, value)| {
                    (key.as_deref() == Some("angle")).then(|| value_to_hyprlang(value)).flatten()
                });
                return Some(match angle { Some(angle) => format!("{colors} {angle}deg"), None => colors });
            }
            let values = entries.iter().filter_map(|(key, value)| {
                if key.is_some() { None } else { value_to_hyprlang(value) }
            }).collect::<Vec<_>>();
            if values.is_empty() { None } else { Some(values.join(", ")) }
        }
    }
}

fn table_string(table: &LuaValue, key: &str) -> Option<String> {
    let LuaValue::Table(entries) = table else { return None; };
    entries.iter().find_map(|(entry_key, value)| {
        if entry_key.as_deref() == Some(key) { value_to_hyprlang(value) } else { None }
    })
}

fn table_bool(table: &LuaValue, key: &str) -> Option<bool> {
    match table_string(table, key)?.as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn parse_table_arg(arg: &str) -> Option<LuaValue> {
    let mut cursor = 0;
    parse_value(arg, &mut cursor).filter(|value| matches!(value, LuaValue::Table(_)))
}

fn parse_value(source: &str, cursor: &mut usize) -> Option<LuaValue> {
    skip_trivia(source, cursor);
    let bytes = source.as_bytes();
    match *bytes.get(*cursor)? {
        b'{' => parse_table(source, cursor),
        b'\'' | b'"' => read_lua_string(source, cursor).map(LuaValue::Scalar),
        _ => {
            let start = *cursor;
            let mut round = 0usize;
            let mut square = 0usize;
            let mut curly = 0usize;
            while *cursor < bytes.len() {
                let byte = bytes[*cursor];
                if byte == b'\'' || byte == b'"' {
                    let _ = read_lua_string(source, cursor)?;
                    continue;
                }
                if byte == b'-' && bytes.get(*cursor + 1) == Some(&b'-') {
                    skip_comment(source, cursor);
                    continue;
                }
                match byte {
                    b'(' => round += 1,
                    b')' => round = round.saturating_sub(1),
                    b'[' => square += 1,
                    b']' => square = square.saturating_sub(1),
                    b'{' => curly += 1,
                    b'}' if round == 0 && square == 0 && curly == 0 => break,
                    b'}' => curly = curly.saturating_sub(1),
                    b',' | b';' if round == 0 && square == 0 && curly == 0 => break,
                    _ => {}
                }
                *cursor += 1;
            }
            let raw = source[start..*cursor].trim();
            if raw.is_empty() { None } else { Some(LuaValue::Scalar(raw.to_string())) }
        }
    }
}

fn parse_table(source: &str, cursor: &mut usize) -> Option<LuaValue> {
    if source.as_bytes().get(*cursor) != Some(&b'{') { return None; }
    *cursor += 1;
    let mut entries = Vec::new();
    loop {
        skip_trivia(source, cursor);
        let byte = *source.as_bytes().get(*cursor)?;
        if byte == b'}' {
            *cursor += 1;
            return Some(LuaValue::Table(entries));
        }
        let start = *cursor;
        let key = parse_table_key(source, cursor);
        if key.is_none() { *cursor = start; }
        let value = parse_value(source, cursor)?;
        entries.push((key, value));
        skip_trivia(source, cursor);
        match source.as_bytes().get(*cursor) {
            Some(b',') | Some(b';') => *cursor += 1,
            Some(b'}') => {}
            Some(_) | None => return None,
        }
    }
}

fn parse_table_key(source: &str, cursor: &mut usize) -> Option<String> {
    skip_trivia(source, cursor);
    if source.as_bytes().get(*cursor) == Some(&b'[') {
        *cursor += 1;
        skip_trivia(source, cursor);
        let key = read_lua_string(source, cursor)?;
        skip_trivia(source, cursor);
        if source.as_bytes().get(*cursor) != Some(&b']') { return None; }
        *cursor += 1;
        skip_trivia(source, cursor);
        if source.as_bytes().get(*cursor) == Some(&b'=') {
            *cursor += 1;
            return Some(key);
        }
        return None;
    }

    let start = *cursor;
    let bytes = source.as_bytes();
    if !bytes.get(*cursor).is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_') {
        return None;
    }
    *cursor += 1;
    while bytes.get(*cursor).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
        *cursor += 1;
    }
    let key = source[start..*cursor].to_string();
    skip_trivia(source, cursor);
    if bytes.get(*cursor) == Some(&b'=') { *cursor += 1; Some(key) } else { None }
}

fn read_lua_string(source: &str, cursor: &mut usize) -> Option<String> {
    let quote = *source.as_bytes().get(*cursor)?;
    if quote != b'\'' && quote != b'"' { return None; }
    *cursor += 1;
    let mut output = String::new();
    while *cursor < source.len() {
        let byte = source.as_bytes()[*cursor];
        *cursor += 1;
        if byte == quote { return Some(output); }
        if byte == b'\\' {
            let escaped = *source.as_bytes().get(*cursor)?;
            *cursor += 1;
            output.push(match escaped {
                b'n' => '\n', b'r' => '\r', b't' => '\t', b'\\' => '\\',
                b'\'' => '\'', b'"' => '"', other => other as char,
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

fn parse_string_literal(source: &str) -> Option<String> {
    let mut cursor = 0;
    skip_trivia(source, &mut cursor);
    read_lua_string(source, &mut cursor)
}

fn skip_trivia(source: &str, cursor: &mut usize) {
    let bytes = source.as_bytes();
    loop {
        while bytes.get(*cursor).is_some_and(|byte| byte.is_ascii_whitespace()) { *cursor += 1; }
        if bytes.get(*cursor) == Some(&b'-') && bytes.get(*cursor + 1) == Some(&b'-') {
            skip_comment(source, cursor);
        } else { break; }
    }
}

fn skip_comment(source: &str, cursor: &mut usize) {
    let bytes = source.as_bytes();
    *cursor += 2;
    while *cursor < bytes.len() && bytes[*cursor] != b'\n' { *cursor += 1; }
}

fn calls(source: &str) -> Vec<LuaCall> {
    let bytes = source.as_bytes();
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'-' && bytes.get(cursor + 1) == Some(&b'-') {
            skip_comment(source, &mut cursor);
            continue;
        }
        if bytes[cursor] == b'\'' || bytes[cursor] == b'"' {
            if read_lua_string(source, &mut cursor).is_none() { break; }
            continue;
        }
        if bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_' {
            let start = cursor;
            cursor += 1;
            while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'.') {
                cursor += 1;
            }
            let name = &source[start..cursor];
            let mut open = cursor;
            skip_trivia(source, &mut open);
            if (name == "require" || name.starts_with("hl."))
                && bytes.get(open) == Some(&b'(')
                && let Some(close) = find_matching(source, open, '(', ')')
            {
                output.push(LuaCall { name: name.to_string(), args: source[open + 1..close].to_string() });
                cursor = close + 1;
            }
            continue;
        }
        cursor += 1;
    }
    output
}

fn find_matching(source: &str, open: usize, left: char, right: char) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 1usize;
    let mut cursor = open + 1;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'\'' || byte == b'"' {
            read_lua_string(source, &mut cursor)?;
            continue;
        }
        if byte == b'-' && bytes.get(cursor + 1) == Some(&b'-') {
            skip_comment(source, &mut cursor);
            continue;
        }
        if byte == left as u8 { depth += 1; }
        else if byte == right as u8 {
            depth -= 1;
            if depth == 0 { return Some(cursor); }
        }
        cursor += 1;
    }
    None
}

fn split_top_level(source: &str, delimiter: char) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut cursor = 0usize;
    let mut round = 0usize;
    let mut curly = 0usize;
    let mut square = 0usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'\'' || byte == b'"' {
            if read_lua_string(source, &mut cursor).is_none() { break; }
            continue;
        }
        if byte == b'-' && bytes.get(cursor + 1) == Some(&b'-') {
            skip_comment(source, &mut cursor);
            continue;
        }
        match byte {
            b'(' => round += 1, b')' => round = round.saturating_sub(1),
            b'{' => curly += 1, b'}' => curly = curly.saturating_sub(1),
            b'[' => square += 1, b']' => square = square.saturating_sub(1),
            _ => {}
        }
        if byte == delimiter as u8 && round == 0 && curly == 0 && square == 0 {
            parts.push(source[start..cursor].trim().to_string());
            start = cursor + 1;
        }
        cursor += 1;
    }
    let last = source[start..].trim();
    if !last.is_empty() { parts.push(last.to_string()); }
    parts
}

fn lua_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")
        .replace('\n', "\n").replace('\r', "\\r").replace('\t', "\\t"))
}

fn lua_key(key: &str) -> String {
    if !key.is_empty() && key.chars().enumerate().all(|(index, ch)| {
        ch == '_' || ch.is_ascii_alphanumeric() && (index > 0 || !ch.is_ascii_digit())
    }) { key.to_string() } else { format!("[{}]", lua_quote(key)) }
}

#[derive(Default)]
struct ConfigNode {
    values: BTreeMap<String, String>,
    children: BTreeMap<String, ConfigNode>,
}

impl ConfigNode {
    fn insert(&mut self, path: &[String], key: String, value: String) {
        if let Some((head, tail)) = path.split_first() {
            self.children.entry(head.clone()).or_default().insert(tail, key, value);
        } else { self.values.insert(key, value); }
    }
}

pub fn hyprlang_to_lua(source: &str) -> String {
    let mut root = ConfigNode::default();
    let mut directives = Vec::new();
    let mut section_stack: Vec<String> = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
        if trimmed == "}" { section_stack.pop(); continue; }
        if let Some(section) = trimmed.strip_suffix('{') {
            if !section.trim().is_empty() { section_stack.push(section.trim().to_string()); }
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else { continue; };
        let key = key.trim().to_string();
        let value = value.split('#').next().unwrap_or(value).trim().to_string();
        if section_stack.is_empty() { directives.push((key, value)); }
        else { root.insert(&section_stack, key, value); }
    }

    let mut output = String::from("-- BEGIN HYPRVIZ MANAGED CONFIG\n-- Generated by HyprViz. Changes are managed by the GUI.\n");
    if !root.children.is_empty() || !root.values.is_empty() {
        output.push_str("hl.config({\n");
        for (key, child) in &root.children { serialize_node(key, child, 1, &mut output); }
        for (key, value) in &root.values {
            output.push_str(&format!("    {} = {},\n", lua_key(key), lua_literal(value)));
        }
        output.push_str("})\n");
    }
    for (key, value) in directives {
        if let Some(line) = serialize_directive(&key, &value) { output.push_str(&line); }
        else { output.push_str(&format!("-- HyprViz could not serialize legacy directive: {key} = {value}\n")); }
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
        if !merged.ends_with('\n') { merged.push('\n'); }
        return merged;
    }

    let mut merged = existing.trim_end().to_string();
    if !merged.is_empty() { merged.push_str("\n\n"); }
    merged.push_str(generated.trim());
    merged.push('\n');
    merged
}

fn serialize_node(name: &str, node: &ConfigNode, depth: usize, output: &mut String) {
    let indent = "    ".repeat(depth);
    output.push_str(&format!("{indent}{} = {{\n", lua_key(name)));
    for (key, child) in &node.children { serialize_node(key, child, depth + 1, output); }
    for (key, value) in &node.values {
        output.push_str(&format!("{}{} = {},\n", "    ".repeat(depth + 1), lua_key(key), lua_literal(value)));
    }
    output.push_str(&format!("{indent}}},\n"));
}

fn lua_literal(value: &str) -> String {
    let value = value.trim();
    if value == "true" || value == "false" || value == "nil"
        || value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok()
    { value.to_string() } else { lua_quote(value) }
}

fn serialize_directive(key: &str, value: &str) -> Option<String> {
    let parts = split_legacy_csv(value);
    match key {
        "monitor" => {
            let output = parts.first()?;
            let mode = parts.get(1).map(String::as_str).unwrap_or("preferred");
            let position = parts.get(2).map(String::as_str).unwrap_or("0x0");
            let scale = parts.get(3).map(String::as_str).unwrap_or("1");
            Some(format!("hl.monitor({{ output = {}, mode = {}, position = {}, scale = {} }})\n",
                lua_quote(output), lua_quote(mode), lua_quote(position), lua_literal(scale)))
        }
        "env" => {
            Some(format!("hl.env({}, {})\n", lua_quote(parts.first()?), lua_quote(parts.get(1).map(String::as_str).unwrap_or(""))))
        }
        key if key.starts_with("bind") => serialize_bind_directive(key, &parts),
        _ => None,
    }
}

fn serialize_bind_directive(key: &str, parts: &[String]) -> Option<String> {
    if parts.len() < 3 { return None; }
    let modifiers = parts[0].split('_').filter(|part| !part.is_empty()).collect::<Vec<_>>();
    let key_name = parts[1].as_str();
    let chord = if modifiers.is_empty() { key_name.to_string() }
        else { format!("{} + {key_name}", modifiers.join(" + ")) };
    let dispatcher = parts[2].as_str();
    let args = parts.get(3..).unwrap_or_default().join(",");
    let call = match dispatcher {
        "exec" => format!("hl.dsp.exec_cmd({})", lua_quote(&args)),
        "killactive" => "hl.dsp.window.close()".to_string(),
        "togglefloating" => "hl.dsp.window.toggle_floating()".to_string(),
        "fullscreen" => "hl.dsp.window.toggle_fullscreen()".to_string(),
        "exit" => "hl.dsp.exit()".to_string(),
        _ => return None,
    };
    let mut flags = Vec::new();
    if key.contains('l') { flags.push("locked = true"); }
    if key.contains('e') { flags.push("repeat = true"); }
    if key.contains('m') { flags.push("mouse = true"); }
    let opts = if flags.is_empty() { String::new() } else { format!(", {{ {} }}", flags.join(", ")) };
    Some(format!("hl.bind({}, {}{})\n", lua_quote(&chord), call, opts))
}

fn split_legacy_csv(value: &str) -> Vec<String> {
    value.split(',').map(|part| part.trim().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_hl_config_tables() {
        let source = r#"
            hl.config({
                general = { gaps_in = 5, gaps_out = 10 },
                decoration = { blur = { enabled = true, size = 4 } },
                ["input.sensitivity"] = -0.2,
            })
        "#;
        let parsed = lua_to_hyprlang(source);
        assert!(parsed.contains("general {"));
        assert!(parsed.contains("gaps_in = 5"));
        assert!(parsed.contains("decoration {"));
        assert!(parsed.contains("blur {"));
        assert!(parsed.contains("enabled = true"));
        assert!(parsed.contains("input {"));
        assert!(parsed.contains("sensitivity = -0.2"));
    }

    #[test]
    fn ignores_calls_in_comments_and_strings() {
        let source = r#"
            -- hl.config({ general = { gaps_in = 999 } })
            local text = "hl.config({ general = { gaps_in = 999 } })"
            hl.config({ general = { gaps_in = 5 } })
        "#;
        let parsed = lua_to_hyprlang(source);
        assert!(parsed.contains("gaps_in = 5"));
        assert!(!parsed.contains("999"));
    }

    #[test]
    fn handles_profiles_and_bindings() {
        assert_eq!(profile_name_from_lua("require('./hyprviz/custom')"), Some("custom".to_string()));
        assert!(has_hyprviz_require("require('./hyprviz')"));
        let parsed = lua_to_hyprlang(r#"hl.bind("SUPER + Q", hl.dsp.exec_cmd("kitty"))"#);
        assert!(parsed.contains("bind = SUPER,Q,exec,kitty"));
    }

    #[test]
    fn serializes_config_to_lua() {
        let lua = hyprlang_to_lua("general {\n    gaps_in = 5\n}\n");
        assert!(lua.contains("hl.config({"));
        assert!(lua.contains("gaps_in = 5"));
    }
}
