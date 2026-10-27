use std::ffi::OsString;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Clone, Debug)]
pub(crate) struct LaunchOptions {
    pub(crate) background: bool,
    pub(crate) demo: bool,
    pub(crate) demo_reset: bool,
    pub(crate) window_size: (f32, f32),
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            background: false,
            demo: false,
            demo_reset: false,
            window_size: (1280.0, 820.0),
        }
    }
}

/// Timing probes stay silent unless `VRC_PHOTO_TIMING` is set.
pub(crate) fn timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VRC_PHOTO_TIMING").is_some())
}

pub(crate) fn log_timing(label: &str, start: Instant) {
    if timing_enabled() {
        eprintln!("[timing] {label}: {:?}", start.elapsed());
    }
}

/// Process start, captured once so the first-frame probe can report launch
/// latency. Only initialized when timing is enabled.
pub(crate) fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

pub(crate) fn parse_args<I, S>(args: I) -> LaunchOptions
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut options = LaunchOptions::default();
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        let Some(flag) = arg.to_str() else {
            continue;
        };
        match flag {
            "--background" => options.background = true,
            "--demo" => options.demo = true,
            "--demo-reset" => options.demo_reset = true,
            "--window-size" => {
                if let Some(value) = args.next()
                    && let Some(value) = value.to_str()
                    && let Some(size) = parse_window_size(value)
                {
                    options.window_size = size;
                }
            }
            _ => {}
        }
    }
    options
}

pub(crate) fn parse_window_size(value: &str) -> Option<(f32, f32)> {
    let (width, height) = value.split_once(['x', 'X'])?;
    let width: f32 = width.trim().parse().ok()?;
    let height: f32 = height.trim().parse().ok()?;
    if !width.is_finite() || !height.is_finite() || width < 320.0 || height < 240.0 {
        return None;
    }
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::{parse_args, parse_window_size};
    use std::ffi::OsString;

    #[test]
    fn window_size_parser_accepts_wxh() {
        assert_eq!(parse_window_size("1440x900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("1440X900"), Some((1440.0, 900.0)));
        assert_eq!(parse_window_size("0x900"), None);
        assert_eq!(parse_window_size("1440,900"), None);
        assert_eq!(parse_window_size("1440x"), None);
    }

    #[test]
    fn launch_options_parse_demo_and_size() {
        let options = parse_args([
            "app".to_string(),
            "--demo".to_string(),
            "--window-size".to_string(),
            "1440x900".to_string(),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1440.0, 900.0));
    }

    #[test]
    fn launch_options_ignore_unknown_flags() {
        let options = parse_args([
            "app".to_string(),
            "--demo".to_string(),
            "--unexpected".to_string(),
            "--window-size".to_string(),
            "1280x820".to_string(),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1280.0, 820.0));
    }

    #[cfg(unix)]
    #[test]
    fn launch_options_ignore_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;

        let options = parse_args(vec![
            OsString::from("app"),
            OsString::from_vec(vec![0x80]),
            OsString::from("--demo"),
        ]);
        assert!(options.demo);
        assert_eq!(options.window_size, (1280.0, 820.0));
    }
}
