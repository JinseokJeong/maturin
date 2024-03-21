use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{ArgAction, Parser, ValueEnum};
use fs_err as fs;

use crate::build_options::find_bridge;
use crate::project_layout::ProjectResolver;
use crate::{BridgeModel, CargoOptions};

/// CI providers
#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum Provider {
    /// GitHub
    GitHub,
}

/// Platform
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum Platform {
    /// All
    All,
    /// Linux
    Linux,
    /// Windows
    Windows,
    /// macOS
    Macos,
    /// Emscripten
    Emscripten,
    /// macOS(Arm64)
    MacosArm64,
}

impl Platform {
    fn defaults() -> Vec<Self> {
        vec![
            Platform::Linux,
            Platform::Windows,
            Platform::Macos,
            Platform::MacosArm64,
        ]
    }

    fn all() -> Vec<Self> {
        vec![
            Platform::Linux,
            Platform::Windows,
            Platform::Macos,
            Platform::Emscripten,
            Platform::MacosArm64,
        ]
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Platform::All => write!(f, "all"),
            Platform::Linux => write!(f, "linux"),
            Platform::Windows => write!(f, "windows"),
            Platform::Macos => write!(f, "macos"),
            Platform::Emscripten => write!(f, "emscripten"),
            Platform::MacosArm64 => write!(f, "macosarm64"),
        }
    }
}

/// Generate CI configuration
#[derive(Debug, Parser)]
pub struct GenerateCI {
    /// CI provider
    #[arg(value_enum, value_name = "CI")]
    pub ci: Provider,
    /// Path to Cargo.toml
    #[arg(short = 'm', long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    /// Output path
    #[arg(short = 'o', long, value_name = "PATH", default_value = "-")]
    pub output: PathBuf,
    /// Platform support
    #[arg(
        id = "platform",
        long,
        action = ArgAction::Append,
        num_args = 1..,
        default_values_t = vec![Platform::Linux, Platform::Windows, Platform::Macos, Platform::MacosArm64],
    )]
    pub platforms: Vec<Platform>,
    /// Enable pytest
    #[arg(long)]
    pub pytest: bool,
    /// Use zig to do cross compilation
    #[arg(long)]
    pub zig: bool,
}

impl Default for GenerateCI {
    fn default() -> Self {
        Self {
            ci: Provider::GitHub,
            manifest_path: None,
            output: PathBuf::from("-"),
            platforms: vec![
                Platform::Linux,
                Platform::Windows,
                Platform::Macos,
                Platform::MacosArm64,
            ],
            pytest: false,
            zig: false,
        }
    }
}

impl GenerateCI {
    /// Execute this command
    pub fn execute(&self) -> Result<()> {
        let conf = self.generate()?;
        self.print(&conf)
    }

    /// Generate CI configuration
    pub fn generate(&self) -> Result<String> {
        let cargo_options = CargoOptions {
            manifest_path: self.manifest_path.clone(),
            ..Default::default()
        };
        let ProjectResolver {
            cargo_metadata,
            pyproject_toml,
            project_layout,
            ..
        } = ProjectResolver::resolve(self.manifest_path.clone(), cargo_options)?;
        let pyproject = pyproject_toml.as_ref();
        let bridge = find_bridge(&cargo_metadata, pyproject.and_then(|x| x.bindings()))?;
        let project_name = pyproject
            .and_then(|project| project.project_name())
            .unwrap_or(&project_layout.extension_name);
        let sdist = pyproject_toml.is_some();

        match self.ci {
            Provider::GitHub => self.generate_github(project_name, &bridge, sdist),
        }
    }

    pub(crate) fn generate_github(
        &self,
        project_name: &str,
        bridge_model: &BridgeModel,
        sdist: bool,
    ) -> Result<String> {
        let is_abi3 = matches!(bridge_model, BridgeModel::BindingsAbi3(..));
        let is_bin = bridge_model.is_bin();
        let setup_python = self.pytest
            || matches!(
                bridge_model,
                BridgeModel::Bin(Some(_))
                    | BridgeModel::Bindings(..)
                    | BridgeModel::BindingsAbi3(..)
                    | BridgeModel::Cffi
                    | BridgeModel::UniFfi
            );
        let mut gen_cmd = std::env::args()
            .enumerate()
            .map(|(i, arg)| {
                if i == 0 {
                    env!("CARGO_PKG_NAME").to_string()
                } else {
                    arg
                }
            })
            .collect::<Vec<String>>()
            .join(" ");
        if gen_cmd.starts_with("maturin new") || gen_cmd.starts_with("maturin init") {
            gen_cmd = format!("{} generate-ci github", env!("CARGO_PKG_NAME"));
        }
        let mut conf = format!(
            "# This file is autogenerated by maturin v{version}
# To update, run
#
#    {gen_cmd}
#
name: CI

on:
  push:
    branches:
      - main
      - master
    tags:
      - '*'
  pull_request:
  workflow_dispatch:

permissions:
  contents: read

jobs:\n",
            version = env!("CARGO_PKG_VERSION"),
        );

        let mut needs = Vec::new();
        let platforms: BTreeSet<_> = self
            .platforms
            .iter()
            .flat_map(|p| {
                if matches!(p, Platform::All) {
                    if !bridge_model.is_bin() {
                        Platform::all()
                    } else {
                        Platform::defaults()
                    }
                } else {
                    vec![*p]
                }
            })
            .collect();
        for platform in &platforms {
            if bridge_model.is_bin() && matches!(platform, Platform::Emscripten) {
                continue;
            }
            let plat_name = match platform {
                Platform::MacosArm64 => "macos_arm64".to_string(),
                _ => platform.to_string(),
            };
            let tag_name = match platform {
                Platform::MacosArm64 => "14".to_string(),
                _ => "latest".to_string(),
            };
            let os_name = match platform {
                Platform::Linux | Platform::Emscripten => "ubuntu",
                Platform::Macos | Platform::MacosArm64 => "macos",
                _ => &plat_name,
            };
            needs.push(plat_name.clone());
            conf.push_str(&format!(
                "  {plat_name}:
    runs-on: {os_name}-{tag_name}\n"
            ));
            // target matrix
            let targets = match platform {
                Platform::Linux => vec!["x86_64", "x86", "aarch64", "armv7", "s390x", "ppc64le"],
                Platform::Windows => vec!["x64", "x86"],
                Platform::Macos => vec!["x86_64"],
                Platform::MacosArm64 => vec!["aarch64"],
                _ => Vec::new(),
            };
            if !targets.is_empty() {
                conf.push_str(&format!(
                    "    strategy:
      matrix:
        target: [{targets}]\n",
                    targets = targets.join(", ")
                ));
            }
            // job steps
            conf.push_str(
                "    steps:
      - uses: actions/checkout@v4\n",
            );

            // install pyodide-build for emscripten
            if matches!(platform, Platform::Emscripten) {
                // install stable pyodide-build
                conf.push_str("      - run: pip install pyodide-build\n");
                // get the current python version for the installed pyodide-build
                conf.push_str(
                    "      - name: Get Emscripten and Python version info
        shell: bash
        run: |
          echo EMSCRIPTEN_VERSION=$(pyodide config get emscripten_version) >> $GITHUB_ENV
          echo PYTHON_VERSION=$(pyodide config get python_version | cut -d '.' -f 1-2) >> $GITHUB_ENV
          pip uninstall -y pyodide-build\n",
                );
                conf.push_str(
                    "      - uses: mymindstorm/setup-emsdk@v12
        with:
          version: ${{ env.EMSCRIPTEN_VERSION }}
          actions-cache-folder: emsdk-cache\n",
                );
                conf.push_str(
                    "      - uses: actions/setup-python@v5
        with:
          python-version: ${{ env.PYTHON_VERSION }}\n",
                );
                // install pyodide-build again in the right Python version
                conf.push_str("      - run: pip install pyodide-build\n");
            } else {
                // setup python on demand
                if setup_python {
                    conf.push_str(
                        "      - uses: actions/setup-python@v5
        with:
          python-version: '3.10'\n",
                    );
                    if matches!(platform, Platform::Windows) {
                        conf.push_str("          architecture: ${{ matrix.target }}\n");
                    }
                }
            }

            // build wheels
            let mut maturin_args = if is_abi3 || (is_bin && !setup_python) {
                Vec::new()
            } else if matches!(platform, Platform::Emscripten) {
                vec!["-i".to_string(), "${{ env.PYTHON_VERSION }}".to_string()]
            } else {
                vec!["--find-interpreter".to_string()]
            };
            if let Some(manifest_path) = self.manifest_path.as_ref() {
                if manifest_path != Path::new("Cargo.toml") {
                    maturin_args.push("--manifest-path".to_string());
                    maturin_args.push(manifest_path.display().to_string())
                }
            }
            if self.zig && matches!(platform, Platform::Linux) {
                maturin_args.push("--zig".to_string());
            }
            let maturin_args = if maturin_args.is_empty() {
                String::new()
            } else {
                format!(" {}", maturin_args.join(" "))
            };
            let maturin_target = if matches!(platform, Platform::Emscripten) {
                "wasm32-unknown-emscripten"
            } else {
                "${{ matrix.target }}"
            };
            conf.push_str(&format!(
                "      - name: Build wheels
        uses: PyO3/maturin-action@v1
        with:
          target: {maturin_target}
          args: --release --out dist{maturin_args}
          sccache: 'true'
"
            ));
            if matches!(platform, Platform::Linux) {
                conf.push_str("          manylinux: auto\n");
            } else if matches!(platform, Platform::Emscripten) {
                conf.push_str("          rust-toolchain: nightly\n");
            }
            // upload wheels
            let artifact_name = if matches!(platform, Platform::Emscripten) {
                String::from("wasm-wheels")
            } else {
                format!("wheels-{platform}-${{{{ matrix.target }}}}")
            };
            conf.push_str(&format!(
                "      - name: Upload wheels
        uses: actions/upload-artifact@v4
        with:
          name: {artifact_name}
          path: dist
"
            ));
            // pytest
            let mut chdir = String::new();
            if let Some(manifest_path) = self.manifest_path.as_ref() {
                if manifest_path != Path::new("Cargo.toml") {
                    let parent = manifest_path.parent().unwrap();
                    chdir = format!("cd {} && ", parent.display());
                }
            }
            if self.pytest {
                if matches!(platform, Platform::Linux) {
                    // Test on host for x86_64
                    conf.push_str(&format!(
                        "      - name: pytest
        if: ${{{{ startsWith(matrix.target, 'x86_64') }}}}
        shell: bash
        run: |
          set -e
          pip install {project_name} --find-links dist --force-reinstall
          pip install pytest
          {chdir}pytest
"
                    ));
                    // Test on QEMU for other architectures
                    conf.push_str(&format!(
                        "      - name: pytest
        if: ${{{{ !startsWith(matrix.target, 'x86') && matrix.target != 'ppc64' }}}}
        uses: uraimo/run-on-arch-action@v2.5.0
        with:
          arch: ${{{{ matrix.target }}}}
          distro: ubuntu22.04
          githubToken: ${{{{ github.token }}}}
          install: |
            apt-get update
            apt-get install -y --no-install-recommends python3 python3-pip
            pip3 install -U pip pytest
          run: |
            set -e
            pip3 install {project_name} --find-links dist --force-reinstall
            {chdir}pytest
"
                    ));
                } else if matches!(platform, Platform::Emscripten) {
                    conf.push_str(
                        "      - uses: actions/setup-node@v3
        with:
          node-version: '18'
",
                    );
                    conf.push_str(&format!(
                        "      - name: pytest
        run: |
          set -e
          pyodide venv .venv
          source .venv/bin/activate
          pip install {project_name} --find-links dist --force-reinstall
          pip install pytest
          {chdir}python -m pytest
"
                    ));
                } else {
                    conf.push_str(&format!(
                        "      - name: pytest
        if: ${{{{ !startsWith(matrix.target, 'aarch64') }}}}
        shell: bash
        run: |
          set -e
          pip install {project_name} --find-links dist --force-reinstall
          pip install pytest
          {chdir}pytest
"
                    ));
                }
            }

            conf.push('\n');
        }

        // build sdist
        if sdist {
            needs.push("sdist".to_string());

            let maturin_args = self
                .manifest_path
                .as_ref()
                .map(|manifest_path| {
                    if manifest_path != Path::new("Cargo.toml") {
                        format!(" --manifest-path {}", manifest_path.display())
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default();
            conf.push_str(&format!(
                r#"  sdist:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Build sdist
        uses: PyO3/maturin-action@v1
        with:
          command: sdist
          args: --out dist{maturin_args}
"#
            ));
            conf.push_str(
                "      - name: Upload sdist
        uses: actions/upload-artifact@v4
        with:
          name: wheels-sdist
          path: dist
",
            );
            conf.push('\n');
        }

        conf.push_str(&format!(
            r#"  release:
    name: Release
    runs-on: ubuntu-latest
    if: "startsWith(github.ref, 'refs/tags/')"
    needs: [{needs}]
"#,
            needs = needs.join(", ")
        ));
        if platforms.contains(&Platform::Emscripten) {
            conf.push_str(
                r#"    permissions:
      # Used to upload release artifacts
      contents: write
"#,
            );
        }
        conf.push_str(
            r#"    steps:
      - uses: actions/download-artifact@v4
      - name: Publish to PyPI
        uses: PyO3/maturin-action@v1
        env:
          MATURIN_PYPI_TOKEN: ${{ secrets.PYPI_API_TOKEN }}
        with:
          command: upload
          args: --non-interactive --skip-existing wheels-*/*
"#,
        );
        if platforms.contains(&Platform::Emscripten) {
            conf.push_str(
                "      - name: Upload to GitHub Release
        uses: softprops/action-gh-release@v1
        with:
          files: |
            wasm-wheels/*.whl
          prerelease: ${{ contains(github.ref, 'alpha') || contains(github.ref, 'beta') }}
",
            );
        }
        Ok(conf)
    }

    fn print(&self, conf: &str) -> Result<()> {
        if self.output == Path::new("-") {
            print!("{conf}");
        } else {
            fs::write(&self.output, conf)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::GenerateCI;
    use crate::BridgeModel;
    use expect_test::expect;

    #[test]
    fn test_generate_github() {
        let conf = GenerateCI::default()
            .generate_github(
                "example",
                &BridgeModel::Bindings("pyo3".to_string(), 7),
                true,
            )
            .unwrap()
            .lines()
            .skip(5)
            .collect::<Vec<_>>()
            .join("\n");
        let expected = expect![[r#"
            name: CI

            on:
              push:
                branches:
                  - main
                  - master
                tags:
                  - '*'
              pull_request:
              workflow_dispatch:

            permissions:
              contents: read

            jobs:
              linux:
                runs-on: ubuntu-latest
                strategy:
                  matrix:
                    target: [x86_64, x86, aarch64, armv7, s390x, ppc64le]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                      manylinux: auto
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-linux-${{ matrix.target }}
                      path: dist

              windows:
                runs-on: windows-latest
                strategy:
                  matrix:
                    target: [x64, x86]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                      architecture: ${{ matrix.target }}
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-windows-${{ matrix.target }}
                      path: dist

              macos:
                runs-on: macos-latest
                strategy:
                  matrix:
                    target: [x86_64]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macos-${{ matrix.target }}
                      path: dist

              macos_arm64:
                runs-on: macos-14
                strategy:
                  matrix:
                    target: [aarch64]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macosarm64-${{ matrix.target }}
                      path: dist

              sdist:
                runs-on: ubuntu-latest
                steps:
                  - uses: actions/checkout@v4
                  - name: Build sdist
                    uses: PyO3/maturin-action@v1
                    with:
                      command: sdist
                      args: --out dist
                  - name: Upload sdist
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-sdist
                      path: dist

              release:
                name: Release
                runs-on: ubuntu-latest
                if: "startsWith(github.ref, 'refs/tags/')"
                needs: [linux, windows, macos, macos_arm64, sdist]
                steps:
                  - uses: actions/download-artifact@v4
                  - name: Publish to PyPI
                    uses: PyO3/maturin-action@v1
                    env:
                      MATURIN_PYPI_TOKEN: ${{ secrets.PYPI_API_TOKEN }}
                    with:
                      command: upload
                      args: --non-interactive --skip-existing wheels-*/*"#]];
        expected.assert_eq(&conf);
    }

    #[test]
    fn test_generate_github_abi3() {
        let conf = GenerateCI::default()
            .generate_github("example", &BridgeModel::BindingsAbi3(3, 7), false)
            .unwrap()
            .lines()
            .skip(5)
            .collect::<Vec<_>>()
            .join("\n");
        let expected = expect![[r#"
            name: CI

            on:
              push:
                branches:
                  - main
                  - master
                tags:
                  - '*'
              pull_request:
              workflow_dispatch:

            permissions:
              contents: read

            jobs:
              linux:
                runs-on: ubuntu-latest
                strategy:
                  matrix:
                    target: [x86_64, x86, aarch64, armv7, s390x, ppc64le]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                      manylinux: auto
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-linux-${{ matrix.target }}
                      path: dist

              windows:
                runs-on: windows-latest
                strategy:
                  matrix:
                    target: [x64, x86]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                      architecture: ${{ matrix.target }}
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-windows-${{ matrix.target }}
                      path: dist

              macos:
                runs-on: macos-latest
                strategy:
                  matrix:
                    target: [x86_64]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macos-${{ matrix.target }}
                      path: dist

              macos_arm64:
                runs-on: macos-14
                strategy:
                  matrix:
                    target: [aarch64]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macosarm64-${{ matrix.target }}
                      path: dist

              release:
                name: Release
                runs-on: ubuntu-latest
                if: "startsWith(github.ref, 'refs/tags/')"
                needs: [linux, windows, macos, macos_arm64]
                steps:
                  - uses: actions/download-artifact@v4
                  - name: Publish to PyPI
                    uses: PyO3/maturin-action@v1
                    env:
                      MATURIN_PYPI_TOKEN: ${{ secrets.PYPI_API_TOKEN }}
                    with:
                      command: upload
                      args: --non-interactive --skip-existing wheels-*/*"#]];
        expected.assert_eq(&conf);
    }

    #[test]
    fn test_generate_github_zig_pytest() {
        let gen = GenerateCI {
            zig: true,
            pytest: true,
            ..Default::default()
        };
        let conf = gen
            .generate_github(
                "example",
                &BridgeModel::Bindings("pyo3".to_string(), 7),
                true,
            )
            .unwrap()
            .lines()
            .skip(5)
            .collect::<Vec<_>>()
            .join("\n");
        let expected = expect![[r#"
            name: CI

            on:
              push:
                branches:
                  - main
                  - master
                tags:
                  - '*'
              pull_request:
              workflow_dispatch:

            permissions:
              contents: read

            jobs:
              linux:
                runs-on: ubuntu-latest
                strategy:
                  matrix:
                    target: [x86_64, x86, aarch64, armv7, s390x, ppc64le]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter --zig
                      sccache: 'true'
                      manylinux: auto
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-linux-${{ matrix.target }}
                      path: dist
                  - name: pytest
                    if: ${{ startsWith(matrix.target, 'x86_64') }}
                    shell: bash
                    run: |
                      set -e
                      pip install example --find-links dist --force-reinstall
                      pip install pytest
                      pytest
                  - name: pytest
                    if: ${{ !startsWith(matrix.target, 'x86') && matrix.target != 'ppc64' }}
                    uses: uraimo/run-on-arch-action@v2.5.0
                    with:
                      arch: ${{ matrix.target }}
                      distro: ubuntu22.04
                      githubToken: ${{ github.token }}
                      install: |
                        apt-get update
                        apt-get install -y --no-install-recommends python3 python3-pip
                        pip3 install -U pip pytest
                      run: |
                        set -e
                        pip3 install example --find-links dist --force-reinstall
                        pytest

              windows:
                runs-on: windows-latest
                strategy:
                  matrix:
                    target: [x64, x86]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                      architecture: ${{ matrix.target }}
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-windows-${{ matrix.target }}
                      path: dist
                  - name: pytest
                    if: ${{ !startsWith(matrix.target, 'aarch64') }}
                    shell: bash
                    run: |
                      set -e
                      pip install example --find-links dist --force-reinstall
                      pip install pytest
                      pytest

              macos:
                runs-on: macos-latest
                strategy:
                  matrix:
                    target: [x86_64]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macos-${{ matrix.target }}
                      path: dist
                  - name: pytest
                    if: ${{ !startsWith(matrix.target, 'aarch64') }}
                    shell: bash
                    run: |
                      set -e
                      pip install example --find-links dist --force-reinstall
                      pip install pytest
                      pytest

              macos_arm64:
                runs-on: macos-14
                strategy:
                  matrix:
                    target: [aarch64]
                steps:
                  - uses: actions/checkout@v4
                  - uses: actions/setup-python@v5
                    with:
                      python-version: '3.10'
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist --find-interpreter
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macosarm64-${{ matrix.target }}
                      path: dist
                  - name: pytest
                    if: ${{ !startsWith(matrix.target, 'aarch64') }}
                    shell: bash
                    run: |
                      set -e
                      pip install example --find-links dist --force-reinstall
                      pip install pytest
                      pytest

              sdist:
                runs-on: ubuntu-latest
                steps:
                  - uses: actions/checkout@v4
                  - name: Build sdist
                    uses: PyO3/maturin-action@v1
                    with:
                      command: sdist
                      args: --out dist
                  - name: Upload sdist
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-sdist
                      path: dist

              release:
                name: Release
                runs-on: ubuntu-latest
                if: "startsWith(github.ref, 'refs/tags/')"
                needs: [linux, windows, macos, macos_arm64, sdist]
                steps:
                  - uses: actions/download-artifact@v4
                  - name: Publish to PyPI
                    uses: PyO3/maturin-action@v1
                    env:
                      MATURIN_PYPI_TOKEN: ${{ secrets.PYPI_API_TOKEN }}
                    with:
                      command: upload
                      args: --non-interactive --skip-existing wheels-*/*"#]];
        expected.assert_eq(&conf);
    }

    #[test]
    fn test_generate_github_bin_no_binding() {
        let conf = GenerateCI::default()
            .generate_github("example", &BridgeModel::Bin(None), true)
            .unwrap()
            .lines()
            .skip(5)
            .collect::<Vec<_>>()
            .join("\n");
        let expected = expect![[r#"
            name: CI

            on:
              push:
                branches:
                  - main
                  - master
                tags:
                  - '*'
              pull_request:
              workflow_dispatch:

            permissions:
              contents: read

            jobs:
              linux:
                runs-on: ubuntu-latest
                strategy:
                  matrix:
                    target: [x86_64, x86, aarch64, armv7, s390x, ppc64le]
                steps:
                  - uses: actions/checkout@v4
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                      manylinux: auto
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-linux-${{ matrix.target }}
                      path: dist

              windows:
                runs-on: windows-latest
                strategy:
                  matrix:
                    target: [x64, x86]
                steps:
                  - uses: actions/checkout@v4
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-windows-${{ matrix.target }}
                      path: dist

              macos:
                runs-on: macos-latest
                strategy:
                  matrix:
                    target: [x86_64]
                steps:
                  - uses: actions/checkout@v4
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macos-${{ matrix.target }}
                      path: dist

              macos_arm64:
                runs-on: macos-14
                strategy:
                  matrix:
                    target: [aarch64]
                steps:
                  - uses: actions/checkout@v4
                  - name: Build wheels
                    uses: PyO3/maturin-action@v1
                    with:
                      target: ${{ matrix.target }}
                      args: --release --out dist
                      sccache: 'true'
                  - name: Upload wheels
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-macosarm64-${{ matrix.target }}
                      path: dist

              sdist:
                runs-on: ubuntu-latest
                steps:
                  - uses: actions/checkout@v4
                  - name: Build sdist
                    uses: PyO3/maturin-action@v1
                    with:
                      command: sdist
                      args: --out dist
                  - name: Upload sdist
                    uses: actions/upload-artifact@v4
                    with:
                      name: wheels-sdist
                      path: dist

              release:
                name: Release
                runs-on: ubuntu-latest
                if: "startsWith(github.ref, 'refs/tags/')"
                needs: [linux, windows, macos, macos_arm64, sdist]
                steps:
                  - uses: actions/download-artifact@v4
                  - name: Publish to PyPI
                    uses: PyO3/maturin-action@v1
                    env:
                      MATURIN_PYPI_TOKEN: ${{ secrets.PYPI_API_TOKEN }}
                    with:
                      command: upload
                      args: --non-interactive --skip-existing wheels-*/*"#]];
        expected.assert_eq(&conf);
    }
}
