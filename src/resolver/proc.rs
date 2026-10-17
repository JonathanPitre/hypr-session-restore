use std::path::Path;

use crate::core::quote::shell_quote;

const INTERPRETER_PREFIXES: &[&str] = &[
    "python3", "python", "node", "electron", "ruby", "perl", "java", "sh", "bash", "zsh", "fish",
    "env",
];

/// Try to resolve a launch command from /proc/<pid>/cmdline.
pub fn resolve_from_proc(pid: i64) -> Option<String> {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let raw = std::fs::read(&cmdline_path).ok()?;

    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).to_string())
        .collect();

    resolve_from_args(&args, &|name| which_exists(name))
}

/// Pure logic: given a parsed cmdline arg list, determine the launch command.
/// `which_fn` abstracts away PATH lookup for testability.
pub fn resolve_from_args(args: &[String], which_fn: &dyn Fn(&str) -> bool) -> Option<String> {
    if args.is_empty() {
        return None;
    }

    let mut start = 0;
    for (i, arg) in args.iter().enumerate() {
        let basename = Path::new(arg)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();

        if INTERPRETER_PREFIXES.contains(&basename.as_ref()) {
            start = i + 1;
            continue;
        }

        if arg.starts_with('-') && start == i {
            start = i + 1;
            continue;
        }

        break;
    }

    if start >= args.len() {
        return None;
    }

    let program = &args[start];
    let basename = Path::new(program)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if program.starts_with('/') {
        if which_fn(&basename) {
            return Some(shell_quote(&basename));
        }
        return Some(shell_quote(program));
    }

    if which_fn(&basename) {
        return Some(shell_quote(&basename));
    }

    Some(shell_quote(program))
}

fn which_exists(name: &str) -> bool {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let full = Path::new(dir).join(name);
            if full.exists() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(val: &str) -> String {
        val.to_string()
    }

    fn no_which(_: &str) -> bool {
        false
    }

    fn always_which(_: &str) -> bool {
        true
    }

    #[test]
    fn empty_args() {
        assert_eq!(resolve_from_args(&[], &no_which), None);
    }

    #[test]
    fn simple_binary() {
        let args = vec![s("firefox")];
        assert_eq!(resolve_from_args(&args, &no_which), Some(s("firefox")));
    }

    #[test]
    fn absolute_path_in_path() {
        let args = vec![s("/usr/bin/firefox")];
        assert_eq!(resolve_from_args(&args, &always_which), Some(s("firefox")));
    }

    #[test]
    fn absolute_path_not_in_path() {
        let args = vec![s("/opt/custom/myapp")];
        assert_eq!(
            resolve_from_args(&args, &no_which),
            Some(s("/opt/custom/myapp"))
        );
    }

    #[test]
    fn skip_python_interpreter() {
        let args = vec![s("/usr/bin/python3"), s("-u"), s("my_app.py")];
        assert_eq!(resolve_from_args(&args, &no_which), Some(s("my_app.py")));
    }

    #[test]
    fn skip_node_interpreter() {
        let args = vec![s("/usr/bin/node"), s("/opt/app/server.js")];
        assert_eq!(
            resolve_from_args(&args, &no_which),
            Some(s("/opt/app/server.js"))
        );
    }

    #[test]
    fn skip_env_then_python() {
        let args = vec![s("/usr/bin/env"), s("python3"), s("-u"), s("script.py")];
        assert_eq!(resolve_from_args(&args, &no_which), Some(s("script.py")));
    }

    #[test]
    fn skip_electron() {
        let args = vec![
            s("/usr/lib/electron/electron"),
            s("--some-flag"),
            s("/usr/share/app/resources/app.asar"),
        ];
        // "electron" basename is in interpreter list, so skip it.
        // "--some-flag" starts with '-' and start==i, so skip.
        // Then we get the .asar path.
        assert_eq!(
            resolve_from_args(&args, &no_which),
            Some(s("/usr/share/app/resources/app.asar"))
        );
    }

    #[test]
    fn skip_bash_with_flags() {
        let args = vec![s("/usr/bin/bash"), s("-c"), s("my_script")];
        // bash is interpreter, skip. -c has start==i, skip. Then "my_script".
        assert_eq!(resolve_from_args(&args, &no_which), Some(s("my_script")));
    }

    #[test]
    fn only_interpreter_no_program() {
        let args = vec![s("/usr/bin/python3")];
        assert_eq!(resolve_from_args(&args, &no_which), None);
    }

    #[test]
    fn only_interpreter_and_flags() {
        let args = vec![s("python3"), s("-u")];
        assert_eq!(resolve_from_args(&args, &no_which), None);
    }

    #[test]
    fn binary_with_arguments() {
        let args = vec![s("ghostty"), s("--some-option=value")];
        assert_eq!(resolve_from_args(&args, &no_which), Some(s("ghostty")));
    }
}
