//! The LaunchDaemon that keeps the anchor loaded: the property list the host step puts in
//! `/Library/LaunchDaemons`. It runs `install` at boot and again every `interval`
//! seconds, so a rule set that something reloaded (an OS update replaces `/etc/pf.conf`),
//! or an install that ran before the system's own pf start-up, is put back. `install` is
//! safe to repeat.

use std::fmt::Write as _;

/// The daemon's label, and the name of its plist file without `.plist`.
pub const LABEL: &str = "com.komira.js.pf-pool";
/// Where the daemon's output goes.
pub const LOG_PATH: &str = "/var/log/js-mac-provision.log";

/// The property list of the daemon that runs `program` with `args` at boot and every
/// `interval_secs` seconds.
#[must_use]
pub fn render_plist(program: &str, args: &[String], interval_secs: u32) -> String {
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let _ = writeln!(text, "\t<key>Label</key>\n\t<string>{LABEL}</string>");
    text.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        let _ = writeln!(text, "\t\t<string>{}</string>", escape(arg));
    }
    text.push_str("\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n");
    let _ = writeln!(
        text,
        "\t<key>StartInterval</key>\n\t<integer>{interval_secs}</integer>"
    );
    let _ = writeln!(
        text,
        "\t<key>StandardErrorPath</key>\n\t<string>{LOG_PATH}</string>"
    );
    text.push_str("</dict>\n</plist>\n");
    text
}

/// `text` as XML character data.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_runs_install_at_boot_and_every_interval() {
        let args: Vec<String> = ["install", "--uid-start", "600", "--uid-count", "100"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let golden = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>com.komira.js.pf-pool</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/usr/local/bin/js-mac-provision</string>
\t\t<string>install</string>
\t\t<string>--uid-start</string>
\t\t<string>600</string>
\t\t<string>--uid-count</string>
\t\t<string>100</string>
\t</array>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>StartInterval</key>
\t<integer>300</integer>
\t<key>StandardErrorPath</key>
\t<string>/var/log/js-mac-provision.log</string>
</dict>
</plist>
";
        assert_eq!(
            render_plist("/usr/local/bin/js-mac-provision", &args, 300),
            golden
        );
    }

    #[test]
    fn xml_characters_in_an_argument_are_escaped() {
        let text = render_plist("/a&b/<p>", &["x>y".to_owned()], 60);
        assert!(text.contains("<string>/a&amp;b/&lt;p&gt;</string>"));
        assert!(text.contains("<string>x&gt;y</string>"));
        assert!(!text.contains("<p>"));
    }
}
