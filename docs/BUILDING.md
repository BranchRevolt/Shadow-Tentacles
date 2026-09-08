# Building from source

Most people want a ready installer from the
[releases page](https://github.com/BranchRevolt/Shadow-Tentacles/releases). Build
from source if you want to change the program, or if there is no installer for
your platform.

Nothing here needs Node or npm. The interface is plain HTML, CSS and JavaScript,
and it is compiled into the binary along with everything else.

## What the build needs

| | Linux | Windows | macOS |
|---|---|---|---|
| Rust | stable toolchain | stable toolchain | stable toolchain |
| C++ compiler | GCC or Clang | MSVC (Visual Studio Build Tools) | Xcode command line tools |
| Build system | CMake, Ninja | CMake, Ninja | CMake, Ninja |
| GPU backend | Vulkan SDK | Vulkan SDK | Metal (part of macOS) |
| Webview | webkit2gtk 4.1, GTK 3 | WebView2 (part of Windows) | WebKit (part of macOS) |
| Audio | ALSA headers | included | included |

Two dependencies are compiled from source on the first build and then cached:
llama.cpp, which runs the language model, and espeak-ng, which turns text into
the phonemes a voice reads. A third, ONNX Runtime, is downloaded as a prebuilt
library when the voice engine is first compiled.

## Linux

Arch and derivatives:

```sh
sudo pacman -S --needed rust cmake ninja clang \
    vulkan-headers shaderc glslang \
    webkit2gtk-4.1 gtk3 alsa-lib
```

Debian and Ubuntu:

```sh
sudo apt install build-essential cmake ninja-build curl wget file \
    libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
    libayatana-appindicator3-dev libasound2-dev
```

Ubuntu 22.04 has no `glslc` package, so the Vulkan shader compiler comes from
LunarG's repository:

```sh
wget -qO- https://packages.lunarg.com/lunarg-signing-key-pub.asc \
    | sudo tee /etc/apt/trusted.gpg.d/lunarg.asc > /dev/null
sudo wget -qO /etc/apt/sources.list.d/lunarg-vulkan-jammy.list \
    https://packages.lunarg.com/vulkan/lunarg-vulkan-jammy.list
sudo apt update && sudo apt install vulkan-sdk
```

## Windows

Install the Visual Studio Build Tools with the C++ workload, then Ninja and the
Vulkan SDK:

```powershell
choco install ninja -y
```

Download the Vulkan SDK from [LunarG](https://vulkan.lunarg.com/sdk/home) and
point `VULKAN_SDK` at the versioned directory it installs into, such as
`C:\VulkanSDK\1.4.357.0`, not at `C:\VulkanSDK`.

Two settings save two long failures:

```powershell
$env:CMAKE_GENERATOR = "Ninja"
$env:CARGO_TARGET_DIR = "C:\b"
```

The Visual Studio generator cannot build ggml's Vulkan path. It configures
`vulkan-shaders-gen` through `ExternalProject_Add` without a generator, and the
nested run fails with "No CMAKE_CXX_COMPILER could be found". Ninja is what
llama.cpp's own CI uses. The short target directory is for `MAX_PATH`: a nested
CMake probe reaches 261 characters against a limit of 260, and building under
`C:\b` brings it back to roughly 218.

Run the build from a Developer Command Prompt, or from a shell where
`vcvarsall.bat` has run, so that Ninja can find `cl.exe`.

## macOS

```sh
xcode-select --install
brew install cmake ninja
```

Metal is used instead of Vulkan and needs nothing installed. If you build for
Intel, set the deployment target to 10.15 or later. Below that, libc++ marks
`std::filesystem` unavailable and one of ggml's source files will not compile.
The environment variable alone does not reach the compiler, so pass it through a
toolchain file:

```sh
echo 'set(CMAKE_OSX_DEPLOYMENT_TARGET 10.15 CACHE STRING "" FORCE)' > /tmp/osx.cmake
export CMAKE_TOOLCHAIN_FILE=/tmp/osx.cmake
```

## Building

```sh
cargo build --release
```

The binary lands in `target/release/shadow-tentacles`. GPU acceleration is on by
default and chosen per platform, so there is no feature flag to remember. A
machine with no usable GPU falls back to the processor on its own, and
`shadow-tentacles doctor` reports what was found.

Expect roughly seven minutes for a cold build on a modern desktop, almost all of
it llama.cpp and espeak-ng. Later builds take seconds, because both are cached.

## During development

```sh
cargo run      # rebuild and open the window
cargo test     # the suite, which needs no network and no model
```

The interface is compiled in, so a change under `ui/` needs a rebuild like any
other change. On Wayland a development build shows a generic icon, because the
compositor takes the icon from a `.desktop` file rather than from the window.
`packaging/install-dev-desktop.sh` installs one, and removes it again with
`--uninstall`.

Every location the program uses can be overridden while testing:

```sh
env SHADOW_CONFIG_DIR=/tmp/st-config SHADOW_DATA_DIR=/tmp/st-data cargo run
```

`SHADOW_MODELS_DIR` and `SHADOW_VOICES_DIR` work the same way. These are a
developer's door, not the way the program is meant to be configured, and the
window covers every setting a user needs.

## The pinned model runtime

`llama-cpp-2` is pinned to exactly `0.1.146`, and `Cargo.lock` holds
`llama-cpp-sys-2` at the same version. The version selects a vendored copy of
llama.cpp, and from `0.1.154` onward the Vulkan build calls
`find_package(SPIRV-Headers)`, a CMake package ordinary distributions do not
ship. Raising one of the two without the other silently breaks the GPU build, so
change both together and build on a clean machine before keeping the change.

## The vendored voice crate

`vendor/piper-rs` is a copy of the published crate with one line changed, wired in
through `[patch.crates-io]` in the root `Cargo.toml`. It pins `ort` to
`2.0.0-rc.10` rather than `rc.12`: rc.12 moved to ONNX Runtime 1.24.2, whose
prebuilt library requires glibc 2.38 and ships no binary for macOS on Intel, so
with it the Linux and Intel Mac builds cannot link at all. rc.10 uses 1.22.0 and
has neither problem. Both expose the same ort API to this crate.

## Installers

Installers are built by GitHub Actions, not by hand. `.github/workflows/release.yml`
builds for Linux, Windows, macOS on Apple silicon and macOS on Intel, then
attaches the results to a draft release. It runs on a pushed `v*` tag, or from
the Actions tab, where a single platform can be picked to rebuild after a fix.

## See also

- [Architecture](ARCHITECTURE.md), for what the code does once it builds
- [README](../README.md), for using the program
