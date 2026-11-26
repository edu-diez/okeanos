//! Optional OS sandbox for the `run` tool (M30).
//!
//! The sandbox is independent of approvals: it rewrites the command line so the child is
//! spawned through an external helper (`bwrap` on Linux, `sandbox-exec` on macOS). Nothing is
//! linked. If a mode other than `none` is requested but the platform has no helper, `run`
//! fails closed instead of executing unsandboxed.

use crate::tool::tool_trait::ToolError;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::RwLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxMode {
    #[default]
    None,
    ReadOnly,
    WorkspaceWrite,
}

impl FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Self::None),
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            other => Err(format!(
                "invalid sandbox mode '{other}' (expected none, read-only or workspace-write)"
            )),
        }
    }
}

/// Process-wide sandbox settings.
#[derive(Clone, Debug, Default)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub network: bool,
    /// Canonical writable roots (used by `workspace-write`).
    pub roots: Vec<PathBuf>,
}

/// A command line to spawn in place of the requested one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wrapped {
    pub program: String,
    pub args: Vec<String>,
}

/// Rewrites a command so it runs confined.
pub trait Sandbox {
    fn wrap(
        &self,
        program: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<Wrapped, ToolError>;
}

static POLICY: RwLock<Option<SandboxPolicy>> = RwLock::new(None);

/// Install the sandbox policy for this process. Writable roots are canonicalized.
pub fn set_policy(mut policy: SandboxPolicy) {
    policy.roots = policy
        .roots
        .into_iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    *POLICY.write().unwrap_or_else(|e| e.into_inner()) = Some(policy);
}

/// Apply the installed policy (if any) to a command.
pub fn apply(program: &str, args: &[String]) -> Result<Wrapped, ToolError> {
    let policy = POLICY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    apply_with(platform_sandbox().as_ref(), &policy, program, args)
}

fn apply_with(
    sandbox: &dyn Sandbox,
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
) -> Result<Wrapped, ToolError> {
    if policy.mode == SandboxMode::None {
        return Ok(Wrapped {
            program: program.to_string(),
            args: args.to_vec(),
        });
    }
    sandbox.wrap(program, args, policy)
}

/// Human-readable description of the OS sandbox available on this platform,
/// and whether one is usable.
pub fn availability() -> (bool, String) {
    if cfg!(target_os = "linux") {
        if helper_exists("bwrap") {
            (true, "bubblewrap (bwrap) available".into())
        } else {
            (
                false,
                "bubblewrap (bwrap) not found; install it to enable the sandbox".into(),
            )
        }
    } else if cfg!(target_os = "macos") {
        if helper_exists("sandbox-exec") {
            (true, "seatbelt (sandbox-exec) available".into())
        } else {
            (false, "sandbox-exec not found".into())
        }
    } else {
        (
            false,
            "no OS sandbox backend on this platform; `run` is only guarded by permissions and path checks (consider WSL or a container)".into(),
        )
    }
}

fn platform_sandbox() -> Box<dyn Sandbox> {
    if cfg!(target_os = "linux") {
        Box::new(Bwrap)
    } else if cfg!(target_os = "macos") {
        Box::new(Seatbelt)
    } else {
        Box::new(Unsupported)
    }
}

/// Windows and other platforms: no helper available, so an enabled sandbox refuses to run.
struct Unsupported;

impl Sandbox for Unsupported {
    fn wrap(&self, _: &str, _: &[String], _: &SandboxPolicy) -> Result<Wrapped, ToolError> {
        Err(ToolError::ExecutionFailed(
            "permissions.sandbox is enabled but no OS sandbox is available on this platform; \
             set permissions.sandbox = \"none\" to run unsandboxed"
                .into(),
        ))
    }
}

/// Linux: bubblewrap.
struct Bwrap;

impl Sandbox for Bwrap {
    fn wrap(
        &self,
        program: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<Wrapped, ToolError> {
        if !helper_exists("bwrap") {
            return Err(ToolError::ExecutionFailed(
                "permissions.sandbox is enabled but 'bwrap' (bubblewrap) was not found in PATH; \
                 install it or set permissions.sandbox = \"none\""
                    .into(),
            ));
        }
        let mut argv = bwrap_args(policy);
        argv.push("--".into());
        argv.push(program.to_string());
        argv.extend(args.iter().cloned());
        Ok(Wrapped {
            program: "bwrap".into(),
            args: argv,
        })
    }
}

/// bwrap options (without the trailing `-- program args`).
fn bwrap_args(policy: &SandboxPolicy) -> Vec<String> {
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]
    .map(String::from)
    .into();
    if !policy.network {
        a.push("--unshare-net".into());
    }
    if policy.mode == SandboxMode::WorkspaceWrite {
        for root in &policy.roots {
            let r = root.to_string_lossy().into_owned();
            a.extend(["--bind".into(), r.clone(), r.clone()]);
            let git = root.join(".git");
            if git.exists() {
                let g = git.to_string_lossy().into_owned();
                a.extend(["--ro-bind".into(), g.clone(), g]);
            }
            if let Ok(entries) = std::fs::read_dir(root) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                    if name == ".env" || name.starts_with(".env.") {
                        let p = e.path().to_string_lossy().into_owned();
                        a.extend(["--ro-bind".into(), p.clone(), p]);
                    }
                }
            }
        }
    }
    a
}

/// macOS: Seatbelt via `sandbox-exec`.
struct Seatbelt;

impl Sandbox for Seatbelt {
    fn wrap(
        &self,
        program: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<Wrapped, ToolError> {
        if !helper_exists("sandbox-exec") {
            return Err(ToolError::ExecutionFailed(
                "permissions.sandbox is enabled but 'sandbox-exec' was not found; \
                 set permissions.sandbox = \"none\""
                    .into(),
            ));
        }
        let mut argv = vec!["-p".to_string(), seatbelt_profile(policy), program.into()];
        argv.extend(args.iter().cloned());
        Ok(Wrapped {
            program: "sandbox-exec".into(),
            args: argv,
        })
    }
}

fn seatbelt_profile(policy: &SandboxPolicy) -> String {
    let q = |p: &Path| {
        let s = p
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("(subpath \"{s}\")")
    };
    let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    p.push_str(
        "(allow file-write* (literal \"/dev/null\") (literal \"/dev/tty\") \
         (subpath \"/private/tmp\") (subpath \"/private/var/folders\"))\n",
    );
    if policy.mode == SandboxMode::WorkspaceWrite {
        for root in &policy.roots {
            p.push_str(&format!("(allow file-write* {})\n", q(root)));
        }
        // Later rules win: re-protect .git and .env* inside the roots.
        for root in &policy.roots {
            p.push_str(&format!("(deny file-write* {})\n", q(&root.join(".git"))));
            let esc = root
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            p.push_str(&format!(
                "(deny file-write* (regex #\"^{}/\\.env(\\..*)?$\"))\n",
                regex_escape(&esc)
            ));
        }
    }
    if !policy.network {
        p.push_str("(deny network*)\n");
    }
    p
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if ".+*?()[]{}|^$\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn helper_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: SandboxMode, network: bool) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            network,
            roots: vec![],
        }
    }

    struct Fake;
    impl Sandbox for Fake {
        fn wrap(&self, p: &str, a: &[String], _: &SandboxPolicy) -> Result<Wrapped, ToolError> {
            Ok(Wrapped {
                program: "fake".into(),
                args: [vec![p.to_string()], a.to_vec()].concat(),
            })
        }
    }

    #[test]
    fn test_mode_parse() {
        assert_eq!("none".parse(), Ok(SandboxMode::None));
        assert_eq!("read-only".parse(), Ok(SandboxMode::ReadOnly));
        assert_eq!("workspace-write".parse(), Ok(SandboxMode::WorkspaceWrite));
        assert!("full".parse::<SandboxMode>().is_err());
    }

    #[test]
    fn test_none_passes_through_and_enabled_goes_through_sandbox() {
        let args = vec!["x".to_string()];
        let w = apply_with(&Fake, &policy(SandboxMode::None, false), "ls", &args).unwrap();
        assert_eq!(w.program, "ls");
        let w = apply_with(&Fake, &policy(SandboxMode::ReadOnly, false), "ls", &args).unwrap();
        assert_eq!(w.program, "fake");
    }

    #[test]
    fn test_unsupported_fails_closed() {
        let err = apply_with(
            &Unsupported,
            &policy(SandboxMode::ReadOnly, false),
            "ls",
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("no OS sandbox is available"));
        // `none` never consults the sandbox.
        assert!(apply_with(&Unsupported, &policy(SandboxMode::None, false), "ls", &[]).is_ok());
    }

    #[test]
    fn test_bwrap_read_only_and_network() {
        let a = bwrap_args(&policy(SandboxMode::ReadOnly, false));
        assert!(a.contains(&"--unshare-net".to_string()));
        assert!(!a.contains(&"--bind".to_string()));
        let a = bwrap_args(&policy(SandboxMode::ReadOnly, true));
        assert!(!a.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn test_bwrap_workspace_write_binds_root_and_protects_git_and_env() {
        let dir = std::env::temp_dir().join(format!("okeanos-sbx-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".env"), "x").unwrap();
        let p = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            network: false,
            roots: vec![dir.clone()],
        };
        let a = bwrap_args(&p);
        let root = dir.to_string_lossy().into_owned();
        let git = dir.join(".git").to_string_lossy().into_owned();
        let env = dir.join(".env").to_string_lossy().into_owned();
        let pos = |flag: &str, path: &str| {
            a.windows(3)
                .position(|w| w[0] == flag && w[1] == path && w[2] == path)
        };
        let bind = pos("--bind", &root).expect("root bound writable");
        assert!(pos("--ro-bind", &git).unwrap() > bind);
        assert!(pos("--ro-bind", &env).unwrap() > bind);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_seatbelt_profile() {
        let p = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            network: false,
            roots: vec![PathBuf::from("/work/\"proj\"")],
        };
        let s = seatbelt_profile(&p);
        assert!(s.contains("(deny file-write*)"));
        assert!(s.contains("(allow file-write* (subpath \"/work/\\\"proj\\\"\"))"));
        assert!(s.contains("(deny file-write* (subpath \"/work/\\\"proj\\\"/.git\"))"));
        assert!(s.contains("(deny network*)"));
        let s = seatbelt_profile(&policy(SandboxMode::ReadOnly, true));
        assert!(!s.contains("deny network"));
        assert!(!s.contains("/work"));
    }
}
