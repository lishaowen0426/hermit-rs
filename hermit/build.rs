use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str;

use flate2::read::GzDecoder;
use tar::Archive;

fn main() {
	// TODO: Replace with is_some_with once stabilized
	// https://github.com/rust-lang/rust/issues/93050
	let targets_hermit =
		matches!(env::var_os("CARGO_CFG_TARGET_OS"), Some(os) if os == OsStr::new("hermit"));
	let runs_clippy =
		matches!(env::var_os("CARGO_CFG_FEATURE"), Some(os) if os == OsStr::new("cargo-clippy"));
	let is_docs_rs = env::var_os("DOCS_RS").is_some();
	if !targets_hermit || runs_clippy || is_docs_rs {
		return;
	}

	//let kernel_src = KernelSrc::local().unwrap_or_else(KernelSrc::download);
	let kernel_src = KernelSrc::local().unwrap();

	kernel_src.build();
}

struct KernelSrc {
	src_dir: PathBuf,
}

impl KernelSrc {
	fn local() -> Option<Self> {
		let mut src_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
		src_dir.set_file_name("kernel");
		let manifest_path = src_dir.join("Cargo.toml");
		if manifest_path.exists() {
			Some(Self { src_dir })
		} else {
			let fallback_kernel_dir = PathBuf::from("/Users/lsw/Code/hermit/kernel");
			fallback_kernel_dir.exists().then(|| Self {
				src_dir: fallback_kernel_dir,
			})
		}
		//manifest_path.exists().then_some(Self { src_dir })
	}
	#[allow(unused)]
	fn download() -> Self {
		let version = "0.6.7";
		let out_dir = out_dir();
		let src_dir = out_dir.join(format!("kernel-{version}"));

		if !src_dir.exists() {
			let url =
				format!("https://github.com/hermitcore/kernel/archive/refs/tags/v{version}.tar.gz");
			let response = ureq::get(url.as_str()).call().unwrap().into_reader();
			let tar = GzDecoder::new(response);
			let mut archive = Archive::new(tar);
			archive.unpack(src_dir.parent().unwrap()).unwrap();
		}

		Self { src_dir }
	}

	fn build(self) {
		let target_dir = target_dir();
		let manifest_path = self.src_dir.join("Cargo.toml");
		assert!(
			manifest_path.exists(),
			"kernel manifest path `{}` does not exist",
			manifest_path.display()
		);
		let arch = env::var_os("CARGO_CFG_TARGET_ARCH").unwrap();
		let profile = env::var("PROFILE").expect("PROFILE was not set");

		let mut cargo = cargo();

		cargo
			.current_dir(&self.src_dir)
			.arg("run")
			.arg("--package=xtask")
			.arg("--target-dir")
			.arg(&target_dir)
			.arg("--")
			.arg("build")
			.arg("--arch")
			.arg(&arch)
			.args([
				"--profile",
				match profile.as_str() {
					"debug" => "dev",
					profile => profile,
				},
			])
			.arg("--target-dir")
			.arg(&target_dir);

		if has_feature("instrument") {
			cargo.arg("--instrument-mcount");
		}

		if has_feature("randomize-layout") {
			cargo.arg("--randomize-layout");
		}

		// Control enabled features via this crate's features
		cargo.arg("--no-default-features");
		forward_features(
			&mut cargo,
			[
				"acpi", "dhcpv4", "fsgsbase", "pci", "pci-ids", "smp", "tcp", "udp", "trace",
				"vga", "rtl8139", "fs",
			]
			.into_iter(),
		);

		println!("cargo:warning=$ {cargo:?}");
		let status = cargo.status().expect("failed to start kernel build");
		assert!(status.success());

		let lib_location = target_dir
			.join(&arch)
			.join(&profile)
			.canonicalize()
			.unwrap();

		println!("cargo:rustc-link-search=native={}", lib_location.display());
		println!("cargo:rustc-link-lib=static=hermit");

		self.rerun_if_changed_cargo(&self.src_dir.join("Cargo.toml"));
		self.rerun_if_changed_cargo(&self.src_dir.join("hermit-builtins/Cargo.toml"));

		println!(
			"cargo:rerun-if-changed={}",
			self.src_dir.join("rust-toolchain.toml").display()
		);

		// HERMIT_LOG_LEVEL_FILTER sets the log level filter at compile time
		println!("cargo:rerun-if-env-changed=HERMIT_LOG_LEVEL_FILTER");
	}

	fn rerun_if_changed_cargo(&self, cargo_toml: &Path) {
		let mut cargo = cargo();

		let output = cargo
			.arg("tree")
			.arg(format!("--manifest-path={}", cargo_toml.display()))
			.arg("--prefix=none")
			.arg("--workspace")
			.output()
			.unwrap();

		let output = str::from_utf8(&output.stdout).unwrap();

		let path_deps = output.lines().filter_map(|dep| {
			let mut split = dep.split(&['(', ')']);
			split.next();
			let path = split.next()?;
			path.starts_with('/').then_some(path)
		});

		for path_dep in path_deps {
			println!("cargo:rerun-if-changed={path_dep}/src");
			println!("cargo:rerun-if-changed={path_dep}/Cargo.toml");
			if Path::new(path_dep).join("Cargo.lock").exists() {
				println!("cargo:rerun-if-changed={path_dep}/Cargo.lock");
			}
			if Path::new(path_dep).join("build.rs").exists() {
				println!("cargo:rerun-if-changed={path_dep}/build.rs");
			}
		}
	}
}

fn cargo() -> Command {
	let cargo = {
		let exe = format!("cargo{}", env::consts::EXE_SUFFIX);
		// On windows, the userspace toolchain ends up in front of the rustup proxy in $PATH.
		// To reach the rustup proxy nonetheless, we explicitly query $CARGO_HOME.
		let mut cargo_home = PathBuf::from(env::var_os("CARGO_HOME").unwrap());
		cargo_home.push("bin");
		cargo_home.push(&exe);
		if cargo_home.exists() {
			cargo_home
		} else {
			PathBuf::from(exe)
		}
	};

	let mut cargo = Command::new(cargo);

	// Remove rust-toolchain-specific environment variables from kernel cargo
	cargo.env_remove("LD_LIBRARY_PATH");
	env::vars()
		.filter(|(key, _value)| key.starts_with("CARGO") || key.starts_with("RUST"))
		.for_each(|(key, _value)| {
			cargo.env_remove(&key);
		});

	cargo
}

fn out_dir() -> PathBuf {
	env::var_os("OUT_DIR").unwrap().into()
}

fn target_dir() -> PathBuf {
	let mut target_dir = out_dir();
	target_dir.push("target");
	target_dir
}

fn has_feature(feature: &str) -> bool {
	let mut var = "CARGO_FEATURE_".to_string();

	var.extend(feature.chars().map(|c| match c {
		'-' => '_',
		c => c.to_ascii_uppercase(),
	}));

	env::var_os(&var).is_some()
}

fn forward_features<'a>(cmd: &mut Command, features: impl Iterator<Item = &'a str>) {
	let features = features.filter(|f| has_feature(f)).collect::<Vec<_>>();
	if !features.is_empty() {
		cmd.args(["--features", &features.join(" ")]);
	}
}
