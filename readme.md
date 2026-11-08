**this fork covers hyprviz with lua support :D**


<div align='center'>

  [<img src="assets/hyprviz.svg" alt="HyprViz Logo" width="128">](https://github.com/timasoft/hyprviz)

<h2>HyprViz</h2>

[![Rust CI](https://img.shields.io/github/actions/workflow/status/timasoft/hyprviz/rust.yml?branch=main&label=Rust%20CI)](https://github.com/timasoft/hyprviz/actions)
[![Spellcheck](https://img.shields.io/github/actions/workflow/status/timasoft/hyprviz/typos.yml?branch=main&label=Spellcheck)](https://github.com/timasoft/hyprviz/actions)
[![Hyprland Version](https://img.shields.io/badge/Hyprland-v0.54.2-blue?logo=hyprland)](https://github.com/hyprwm/Hyprland)
![Repo size](https://img.shields.io/github/repo-size/timasoft/hyprviz?color=gray)

**Fork** of [HyprGUI](https://github.com/MarkusVolk/hyprgui) — an unofficial GUI for configuring Hyprland, built with GTK4 and Rust. 🚀🦀<br>
Comes with a custom [hyprparser](https://crates.io/crates/hyprparser) for Hyprland's configuration file. (Rust btw) 🦀

> **Lua configuration:** HyprViz embeds Lua 5.4 and executes configuration code (locals, functions, conditionals, loops, computed tables, `require()`, `dofile()`, and `loadfile()`) instead of statically parsing Lua source. It collects supported declarative `hl.*` calls for the GUI while keeping the main Lua file and user-authored code intact. The editor does not emulate live compositor state or run compositor dispatchers, plugins, timers, or event callbacks. For safe evaluation, general file IO is disabled, only read-only `os` helpers are exposed, and each evaluation is limited to 25 million VM instructions and 128 MiB of Lua memory. GUI-managed settings are stored separately in `hyprviz.lua`.

![Preview](.github/preview.png)

</div>

## Lua configuration

When `hyprland.lua` is present, HyprViz evaluates it with an embedded Lua 5.4 VM. Normal Lua control flow, local functions, computed tables, and local modules are evaluated as code rather than parsed from source text.

A minimal example:

```lua
local gaps = 6
local terminal = "alacritty"

hl.config({
    general = {
        gaps_in = gaps,
        gaps_out = 12,
    },
    decoration = {
        rounding = 8,
    },
})

hl.bind("SUPER + RETURN", hl.dsp.exec_cmd(terminal))
hl.on("hyprland.start", function()
    hl.exec_cmd("swww-daemon")
end)

-- HyprViz's GUI-managed settings are loaded separately.
require("./hyprviz")

```

HyprViz stores GUI-managed values in `hyprviz.lua` and additional profiles in `hyprviz/<profile>.lua`. The main `hyprland.lua` file and Lua code outside the managed block are kept intact when the GUI saves changes. Use `require()` for modules; `dofile()` and `loadfile()` resolve relative paths from the file currently being evaluated. GUI saves convert supported legacy directives to native Hyprland Lua calls such as `hl.config()`, `hl.monitor()`, `hl.bind()`, `hl.animation()`, and rule APIs. Directives without a native serializer are left as explicit migration comments rather than emitted as invalid `hl.raw()` calls.

The editor is a configuration evaluator, not a running compositor. Live window, workspace and monitor queries return empty or `nil` values. Dispatcher calls, plugins, timers and ordinary event callbacks are not run. The `hyprland.start` callback is evaluated only to collect startup `hl.exec_cmd()` entries; these become `exec-once` directives and are never launched by the editor.

For safer evaluation, general-purpose `io` access is disabled, only read-only `os` helpers are exposed, and each config evaluation is capped at 128 MiB of Lua memory and 25 million VM instructions. Lua 5.4 is built from vendored sources, so a system Lua installation is not required.

## Installation

[![Packaging status](https://repology.org/badge/vertical-allrepos/hyprviz.svg)](https://repology.org/project/hyprviz)

### Arch Linux
There are 2 different [AUR](https://aur.archlinux.org) packages available:

- [hyprviz](https://aur.archlinux.org/packages/hyprviz) - Latest release built from source
- [hyprviz-bin](https://aur.archlinux.org/packages/hyprviz-bin) - Latest release in binary form

Install the preferred package with:
```bash
git clone https://aur.archlinux.org/<package>.git
cd <package>
makepkg -si
```

Or, if you're using an [AUR Helper](https://wiki.archlinux.org/title/AUR_helpers), it's even simpler (using [paru](https://github.com/Morganamilo/paru) as an example):
```bash
paru -S <package>
```

### NixOS
This project is packaged in multiple ways for Nix:

#### From nixpkgs
HyprViz is available in nixpkgs. Recommended installation methods:

For temporary usage:
```bash
nix run nixpkgs#hyprviz
```

For system-wide installation in your NixOS configuration:
```nix
{
  environment.systemPackages = with pkgs; [
    hyprviz
  ];
}
```

#### From GitHub flake
Alternatively, you can use the GitHub flake directly.
Make sure you have flakes enabled in Nix.

You can run **hyprviz** directly with:
```bash
nix run github:timasoft/hyprviz
```

If you want to have hyprviz always available in your $PATH:
```bash
nix profile install github:timasoft/hyprviz
```

If you manage your NixOS configuration with flakes, add hyprviz as an input in your flake.nix:
```nix
{
  inputs.hyprviz.url = "github:timasoft/hyprviz";

  outputs = { self, nixpkgs, hyprviz, ... }:
    {
      nixosConfigurations.my-hostname = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./configuration.nix
          {
            environment.systemPackages = [
              hyprviz.packages.x86_64-linux.default
            ];
          }
        ];
      };
    };
}
```

## Building from source
1. Install Rust (preferably `rustup`) through your distro's package or [the official script](https://www.rust-lang.org/tools/install)
2. Install `git`, `pango` and `gtk4`
3. Clone this repository:
`git clone https://github.com/timasoft/hyprviz.git && cd hyprviz`
4. Compile the app with `cargo build --release` or run it directly with `cargo run --release`

## TODO:
- [x] Improve value parser
- [x] Improve colour options
- [x] Create aur repo
- [x] Add default values
- [x] Add support for sourced files
- [x] Update preview
- [x] Add dropdown_int and bool_int
- [x] Add ability to create and switch between custom profiles
- [x] Add system info section
- [x] Add bind section
- [x] Add windowrule section
- [x] Add other top-level config sections from Hyprland
- [x] Add guides to top-level sections
- [x] Add guides to some other sections
- [x] Add hyprviz to nixpkgs
- [x] Add i18n
- [x] Add curve editor
- [x] Add binds editor
- ~~Add monitor layuot editor~~
- [x] Add fancy editors for all options
- [x] Add symlink support
- [x] Add CTRL-Z/CTRL-Y support
- [x] Add confirmation dialog for editing main hyprland.conf
- [x] Add row actions (discard/remove) for individual options
- [x] Optimize startup performance
- [x] Add support for Lua 5.4 configuration
- [ ] Add support for waybar, swaync, hyprlock...
- [x] Improve GUI
- [ ] Replace hyprparser with [hyprlang-rs](https://github.com/spinualexandru/hyprlang-rs)

## Credits:
- [Nyx](https://github.com/nnyyxxxx) - Implementing the parser, rest of the main GUI, and maintaining the hyprgui project
- [Adam](https://github.com/adamperkowski) - Implementing the base GUI, maintaining the AUR packages and the project alongside Nyx
- [Vaxry](https://github.com/vaxerski) - Hyprland
- [rust-gtk](https://github.com/gtk-rs/gtk4-rs) - The GTK4 library
- [Hyprland](https://github.com/hyprwm/Hyprland) - The window manager
