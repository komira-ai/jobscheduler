//! The signing table of the macOS programs of js-agent: the three programs the installer
//! package lays down, the identifier each is signed under, the identity and team that sign
//! them, and the designated requirement each then has.
//!
//! The release signs each program with the Developer ID Application identity of the team
//! ([`APPLICATION_IDENTITY`]) in the workflow `js-agent-macos-pkg.yml`, and
//! [`super::code_signature`] reads the signature back against this table. The privacy
//! profile `packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` grants Full
//! Disk Access to one of the programs, [`FULL_DISK_ACCESS_PROGRAM`], by its installed path and
//! its requirement; [`privacy_profile`] renders the profile from this table, and
//! [`profile_problems`] says how a committed file differs from it, so the file and the table
//! cannot drift apart.
//!
//! The requirement names the identifier, Apple as the anchor, the Developer ID certificates
//! and the team, and no code hash, so every release of a program meets it.

/// The Apple team that owns the signing identity.
pub const TEAM_ID: &str = "U7RTX4XD3X";

/// The Developer ID Application identity the programs are signed with.
pub const APPLICATION_IDENTITY: &str = "Developer ID Application: Komira AI, Inc. (U7RTX4XD3X)";

/// The certificate authorities above a Developer ID Application signature, leaf first as
/// `codesign --display` lists them as `Authority=` lines.
pub const AUTHORITIES: [&str; 3] = [
    APPLICATION_IDENTITY,
    "Developer ID Certification Authority",
    "Apple Root CA",
];

/// The directory the package installs the programs in, which updates replace them in place.
pub const INSTALL_DIR: &str = "/Library/Application Support/js-agent/bin";

/// The program that holds the Full Disk Access grant: the root helper, which is its own
/// launchd job and starts everything it runs with the responsibility disclaimed, so the
/// grant never reaches a job.
pub const FULL_DISK_ACCESS_PROGRAM: &str = "js-mac-session";

/// One program of the package and the identifier it is signed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// The program's file name.
    pub name: &'static str,
    /// The code-signing identifier.
    pub identifier: &'static str,
}

/// The programs of the package, in the order the workflow signs them.
pub const PROGRAMS: [Program; 3] = [
    Program {
        name: "js-agent",
        identifier: "com.komira.js-agent",
    },
    Program {
        name: "js-mac-session",
        identifier: "com.komira.js-agent.session",
    },
    Program {
        name: "js-mac-provision",
        identifier: "com.komira.js-agent.provision",
    },
];

/// The program called `name`, if the package has one.
#[must_use]
pub fn program(name: &str) -> Option<Program> {
    PROGRAMS.into_iter().find(|p| p.name == name)
}

impl Program {
    /// The designated requirement the signature must carry, as `codesign --display
    /// --requirements -` prints it after `designated => `.
    #[must_use]
    pub fn requirement(&self) -> String {
        format!(
            "identifier \"{}\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = {TEAM_ID}",
            self.identifier
        )
    }

    /// Where the package installs the program.
    #[must_use]
    pub fn install_path(&self) -> String {
        format!("{INSTALL_DIR}/{}", self.name)
    }
}

/// The program that holds the grant.
///
/// # Panics
/// Never: [`FULL_DISK_ACCESS_PROGRAM`] is in [`PROGRAMS`], which a test holds.
#[must_use]
pub fn grant_holder() -> Program {
    program(FULL_DISK_ACCESS_PROGRAM).unwrap_or(PROGRAMS[1])
}

/// The profile that grants Full Disk Access to [`grant_holder`], as the file
/// `packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig` must read.
///
/// One `com.apple.TCC.configuration-profile-policy` payload with one `SystemPolicyAllFiles`
/// entry, which names the installed path and the requirement of the holder and allows it.
#[must_use]
pub fn privacy_profile() -> String {
    let holder = grant_holder();
    let path = holder.install_path();
    let requirement = holder.requirement();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>PayloadContent</key>
	<array>
		<dict>
			<key>PayloadDescription</key>
			<string>Grants Full Disk Access to the root helper of js-agent and to nothing else.</string>
			<key>PayloadDisplayName</key>
			<string>js-agent root helper: Full Disk Access</string>
			<key>PayloadIdentifier</key>
			<string>com.komira.js-agent.full-disk-access.policy</string>
			<key>PayloadType</key>
			<string>com.apple.TCC.configuration-profile-policy</string>
			<key>PayloadUUID</key>
			<string>5C0E7A52-6B1D-4B7E-9C47-2D1F3A8E9B10</string>
			<key>PayloadVersion</key>
			<integer>1</integer>
			<key>Services</key>
			<dict>
				<key>SystemPolicyAllFiles</key>
				<array>
					<dict>
						<key>Authorization</key>
						<string>Allow</string>
						<key>CodeRequirement</key>
						<string>{requirement}</string>
						<key>Comment</key>
						<string>The root helper js-mac-session lists the privacy database's directory to learn whether it holds this grant, and manages the accounts native jobs run as. Every process it starts for a job is started with the responsibility disclaimed, so the grant never reaches a job.</string>
						<key>Identifier</key>
						<string>{path}</string>
						<key>IdentifierType</key>
						<string>path</string>
						<key>StaticCode</key>
						<false/>
					</dict>
				</array>
			</dict>
		</dict>
	</array>
	<key>PayloadDescription</key>
	<string>Full Disk Access for the root helper of js-agent, by its installed path and its signature.</string>
	<key>PayloadDisplayName</key>
	<string>js-agent: Full Disk Access</string>
	<key>PayloadIdentifier</key>
	<string>com.komira.js-agent.full-disk-access</string>
	<key>PayloadOrganization</key>
	<string>Komira AI, Inc.</string>
	<key>PayloadScope</key>
	<string>System</string>
	<key>PayloadType</key>
	<string>Configuration</string>
	<key>PayloadUUID</key>
	<string>A3F1D2C4-8E65-4F0B-B7A9-6D52E0C14F38</string>
	<key>PayloadVersion</key>
	<integer>1</integer>
</dict>
</plist>
"#
    )
}

/// How `text` differs from [`privacy_profile`]: one line per differing line of the file, and
/// one for a different length; empty when it is the profile byte for byte.
#[must_use]
pub fn profile_problems(text: &str) -> Vec<String> {
    let want = privacy_profile();
    if text == want {
        return Vec::new();
    }
    let mut found: Vec<String> = text
        .lines()
        .zip(want.lines())
        .enumerate()
        .filter(|(_, (got, want))| got != want)
        .map(|(i, (got, want))| format!("line {}: {got:?}, not {want:?}", i + 1))
        .collect();
    let (have, need) = (text.lines().count(), want.lines().count());
    if have != need {
        found.push(format!("the profile has {have} lines, not {need}"));
    }
    if found.is_empty() {
        found.push("the profile differs in its line ends or its final line".to_owned());
    }
    found
}
