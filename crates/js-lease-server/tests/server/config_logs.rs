//! The `[logs]` table of the configuration file: its default, its refusals key by key, what a
//! good table loads as, that no secret of its key files is printed, and the start log line
//! that names the sink.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use js_lease_server::{Cli, Config, LogsConfig, Minting, ObjectStoreLogs};

use crate::binary::{Guard, launch};
use crate::config_support::{PoolText, file, load, quoted, server};
use crate::support::{Scratch, base_args};

/// The secret of the read key file. It must appear in no output.
const READ_SECRET: &str = "ReadSecretNeverPrinted";

/// The secret of the mint key file. It must appear in no output.
const MINT_SECRET: &str = "MintSecretNeverPrinted";

/// The secret of the signing key file. It must appear in no output.
const SIGNING_SECRET: &str = "SigningSecretNeverPrinted";

/// The program.
const PROGRAM: &str = env!("CARGO_BIN_EXE_js-lease-server");

/// The JSON of a key file with `id` and `secret`.
fn key_json(id: &str, secret: &str) -> String {
    format!("{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"{secret}\"}}\n")
}

/// One `[logs]` table, key by key.
#[derive(Clone)]
struct Logs(Vec<(String, String)>);

impl Logs {
    /// A good object-store table with every required key, its key files written in `scratch`.
    fn good(scratch: &Scratch) -> Self {
        let read = scratch.write("read-key", &key_json("READKEYID", READ_SECRET), 0o600);
        let mint = scratch.write("mint-key", &key_json("MINTKEYID", MINT_SECRET), 0o400);
        Self(Vec::new())
            .set("sink", "\"object_store\"")
            .set("url", "\"s3://logs/js/live\"")
            .set("region", "\"us-east-1\"")
            .set("endpoint", "\"https://objects.example\"")
            .set("read_key_file", &quoted(&read))
            .set("role_arn", "\"arn:aws:iam::example:role/js-logs\"")
            .set("mint_key_file", &quoted(&mint))
    }

    /// The table with `key` set to the TOML value `value`.
    fn set(mut self, key: &str, value: &str) -> Self {
        self.0.retain(|(have, _)| have.as_str() != key);
        self.0.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The table without `key`.
    fn without(mut self, key: &str) -> Self {
        self.0.retain(|(have, _)| have.as_str() != key);
        self
    }

    /// The table as text.
    fn text(&self) -> String {
        let lines: String = self
            .0
            .iter()
            .map(|(key, value)| format!("{key} = {value}\n"))
            .collect();
        format!("[logs]\n{lines}")
    }
}

/// The text of a file of `scratch` with a static pool and then `extra`.
fn file_with(scratch: &Scratch, extra: &str) -> String {
    let pool = PoolText::static_pool(scratch, "hosts");
    format!("{}\n{extra}", file(&server(scratch, ""), &[&pool]))
}

/// A file of `scratch` with a static pool and then `extra`, loaded.
fn loaded(scratch: &Scratch, extra: &str) -> Result<Config, String> {
    load(scratch, &file_with(scratch, extra)).map_err(|err| err.to_string())
}

/// The object-store sink of a file with the table `text`, which must load.
fn sink(scratch: &Scratch, text: &str) -> ObjectStoreLogs {
    match loaded(scratch, text).unwrap().logs() {
        LogsConfig::ObjectStore(sink) => (**sink).clone(),
        LogsConfig::None => panic!("the file should have an object-store sink"),
    }
}

/// The refusal of a file with `logs`.
fn refused(scratch: &Scratch, logs: &Logs) -> String {
    loaded(scratch, &logs.text())
        .err()
        .expect("the file should be refused")
}

/// The STS settings of `sink`.
fn sts(sink: &ObjectStoreLogs) -> (String, String, String) {
    let Minting::Sts {
        endpoint,
        role_arn,
        key,
    } = &sink.minting
    else {
        panic!("the sink mints with sts");
    };
    (
        endpoint.url(),
        role_arn.as_str().to_owned(),
        key.access_key_id().to_owned(),
    )
}

/// The `[logs]` example of `docs/design/configuration.md`, its two key file paths replaced by
/// `read` and `mint`.
fn design_example(read: &Path, mint: &Path) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/configuration.md");
    let text = fs::read_to_string(path).unwrap();
    let rest = &text[text
        .find("## Live logs")
        .expect("the heading is in the page")..];
    let fence = "```toml\n";
    let body = &rest[rest.find(fence).expect("the heading has a toml block") + fence.len()..];
    let example = &body[..body.find("```").unwrap()];
    assert!(example.starts_with("[logs]\n"), "{example}");
    example
        .replace("\"/run/credentials/js/logs-read-key\"", &quoted(read))
        .replace("\"/run/credentials/js/logs-mint-key\"", &quoted(mint))
}

/// Asserts the default: a file with no `[logs]`, an empty one, or `sink = "none"` has no sink,
/// and so has a server run from flags; `sink = "none"` (written or by default) beside any
/// other key is refused naming that key; a sink that is neither value, a sink that is not a
/// string and a `logs` that is not a table are refused naming the key. It catches a stray key
/// that is ignored, and a sink a server would act on without having been asked to.
#[test]
fn logs_defaults_to_none_and_a_none_sink_refuses_other_keys() {
    let scratch = Scratch::new("logs_none");
    for extra in ["", "[logs]\n", "[logs]\nsink = \"none\"\n"] {
        let config = loaded(&scratch, extra).unwrap();
        assert_eq!(config.logs(), &LogsConfig::None, "{extra:?}");
        assert_eq!(config.logs().sink(), "none");
    }
    let flags = Config::from_cli(Cli::try_parse_from(base_args(&scratch)).unwrap()).unwrap();
    assert_eq!(flags.logs(), &LogsConfig::None);

    let none = "not allowed with logs.sink = \"none\"";
    for (extra, said) in [
        (
            "[logs]\nsink = \"none\"\nurl = \"s3://logs/js\"\n",
            format!("logs.url: {none}"),
        ),
        (
            "[logs]\nurl = \"s3://logs/js\"\n",
            format!("logs.url: {none}"),
        ),
        (
            "[logs]\nsink = \"none\"\nmax_follow = 1\nregion = \"us-east-1\"\n",
            format!("logs.region: {none}"),
        ),
        (
            "[logs]\nsink = \"none\"\nmax_follow = 1\n",
            format!("logs.max_follow: {none}"),
        ),
        (
            "[logs]\nsink = \"s3\"\n",
            "logs.sink: must be object_store or none".to_owned(),
        ),
        (
            "[logs]\nsink = 1\n",
            "logs.sink: must be a string".to_owned(),
        ),
    ] {
        assert_eq!(loaded(&scratch, extra).err().unwrap(), said, "{extra}");
    }
    let not_a_table = format!("logs = 5\n{}", file_with(&scratch, ""));
    assert_eq!(
        load(&scratch, &not_a_table).err().unwrap().to_string(),
        "logs: must be a table"
    );
}

/// Asserts each refusal of an object-store table, one key at a time, with the key named and
/// the reason given exactly: the url's scheme, a bucket of 2 and of 64 characters, a prefix
/// with `..`, `.`, an empty segment, a leading or trailing `/`, `*`, `?`, `$` or `{`, or of 513
/// bytes; a missing url, region, read key, role or mint key; a bad region, endpoint, path
/// style, STS endpoint or role; `credentials = "iam"` (the error names the key and both kinds);
/// a `signing_key_file` beside `sts`, given or by default; an STS key (`role_arn`,
/// `sts_endpoint`, `mint_key_file`) beside `presigned_prefix`, and that kind without its signing
/// key; a prefix of 477 bytes, which leaves no room for a host's prefix; a TTL of 14 minutes, of 13 hours and
/// one that is not a duration; `max_follow` of 0 and 4097; a key file that is group-readable,
/// missing or not the key JSON; and an unknown key. It catches any bad value accepted and any
/// error that does not name its key.
#[test]
fn a_logs_table_is_checked_key_by_key() {
    let scratch = Scratch::new("logs_refusals");
    let good = Logs::good(&scratch);
    let url_must = "logs.url: must be s3://BUCKET or s3://BUCKET/PREFIX, the prefix with no leading or trailing /";
    let bucket_must = "logs.url: the bucket must be 3 to 63 bytes of [a-z0-9.-]";
    let prefix_must = "logs.url: the log prefix must be `/`-separated segments of [A-Za-z0-9._-], none empty, `.` or `..`";
    let long_prefix = format!("\"s3://logs/{}/{}\"", "a".repeat(256), "b".repeat(256));
    let mut cases: Vec<(Logs, String)> = vec![
        (good.clone().set("url", "\"https://logs/js\""), url_must.to_owned()),
        (good.clone().set("url", "\"s3://logs/\""), url_must.to_owned()),
        (good.clone().set("url", "\"s3://ab/js\""), bucket_must.to_owned()),
        (
            good.clone().set("url", &format!("\"s3://{}\"", "b".repeat(64))),
            bucket_must.to_owned(),
        ),
        (good.clone().set("url", "\"s3://Logs\""), bucket_must.to_owned()),
        (
            good.clone().set("url", &long_prefix),
            "logs.url: the log prefix is longer than 512 bytes".to_owned(),
        ),
        (good.clone().without("url"), "logs.url: required".to_owned()),
        (good.clone().set("url", "7"), "logs.url: must be a string".to_owned()),
        (good.clone().without("region"), "logs.region: required".to_owned()),
        (
            good.clone().without("endpoint"),
            "logs.endpoint: required".to_owned(),
        ),
        (
            good.clone().set("region", "\"US-EAST\""),
            "logs.region: must be 1 to 64 bytes of [a-z0-9-]".to_owned(),
        ),
        (
            good.clone().set("region", &format!("\"{}\"", "r".repeat(65))),
            "logs.region: must be 1 to 64 bytes of [a-z0-9-]".to_owned(),
        ),
        (
            good.clone().set("region", "\"\""),
            "logs.region: must be 1 to 64 bytes of [a-z0-9-]".to_owned(),
        ),
        (
            good.clone().set("endpoint", "\"ftp://objects.example\""),
            "logs.endpoint: the endpoint URL must begin with https:// or http://".to_owned(),
        ),
        (
            good.clone().set("endpoint", "\"https://user@objects.example\""),
            "logs.endpoint: the endpoint URL must not name a user".to_owned(),
        ),
        (
            good.clone().set("endpoint", "\"https://objects.example?a=1\""),
            "logs.endpoint: the endpoint URL must not have a query".to_owned(),
        ),
        (
            good.clone().set("endpoint", "\"https://objects.example#top\""),
            "logs.endpoint: the endpoint URL must not have a fragment".to_owned(),
        ),
        (
            good.clone().set("endpoint", "\"http://objects.example\""),
            "logs.endpoint: plain http is accepted for a loopback address only".to_owned(),
        ),
        (
            good.clone().set("path_style", "\"yes\""),
            "logs.path_style: must be true or false".to_owned(),
        ),
        (
            good.clone().set("endpoint", "5"),
            "logs.endpoint: must be a string".to_owned(),
        ),
        (
            good.clone().set("credentials", "5"),
            "logs.credentials: must be a string".to_owned(),
        ),
        (
            good.clone().set("sts_endpoint", "5"),
            "logs.sts_endpoint: must be a string".to_owned(),
        ),
        (
            good.clone().without("read_key_file"),
            "logs.read_key_file: required".to_owned(),
        ),
        (
            good.clone().set("credentials", "\"iam\""),
            "logs.credentials: must be sts or presigned_prefix".to_owned(),
        ),
        (
            good.clone().set("signing_key_file", "\"/signing-key\""),
            "logs.signing_key_file: not allowed with logs.credentials = \"sts\"".to_owned(),
        ),
        (
            good.clone()
                .set("credentials", "\"sts\"")
                .set("signing_key_file", "\"/signing-key\""),
            "logs.signing_key_file: not allowed with logs.credentials = \"sts\"".to_owned(),
        ),
        (
            good.clone().set("credentials", "\"presigned_prefix\""),
            "logs.role_arn: not allowed with logs.credentials = \"presigned_prefix\"".to_owned(),
        ),
        (
            good.clone()
                .without("role_arn")
                .without("mint_key_file")
                .set("sts_endpoint", "\"https://sts.example\"")
                .set("credentials", "\"presigned_prefix\""),
            "logs.sts_endpoint: not allowed with logs.credentials = \"presigned_prefix\""
                .to_owned(),
        ),
        (
            good.clone()
                .without("role_arn")
                .set("credentials", "\"presigned_prefix\""),
            "logs.mint_key_file: not allowed with logs.credentials = \"presigned_prefix\""
                .to_owned(),
        ),
        (
            good.clone()
                .without("role_arn")
                .without("mint_key_file")
                .set("credentials", "\"presigned_prefix\""),
            "logs.signing_key_file: required".to_owned(),
        ),
        (
            good.clone()
                .set("url", &format!("\"s3://logs/{}\"", "p".repeat(477))),
            "logs.url: must leave room for a host's prefix under it: a prefix of at most 476 bytes"
                .to_owned(),
        ),
        (
            good.clone().set("sts_endpoint", "\"ftp://sts.example\""),
            "logs.sts_endpoint: the endpoint URL must begin with https:// or http://".to_owned(),
        ),
        (
            good.clone().without("role_arn"),
            "logs.role_arn: required".to_owned(),
        ),
        (
            good.clone().set("role_arn", "\"role/js-logs\""),
            "logs.role_arn: the role must be an arn: value of printable ASCII, at most 2048 bytes"
                .to_owned(),
        ),
        (
            good.clone().without("mint_key_file"),
            "logs.mint_key_file: required".to_owned(),
        ),
        (
            good.clone().set("credential_ttl", "\"14m\""),
            "logs.credential_ttl: must be from 15m to 12h".to_owned(),
        ),
        (
            good.clone().set("credential_ttl", "\"13h\""),
            "logs.credential_ttl: must be from 15m to 12h".to_owned(),
        ),
        (
            good.clone().set("credential_ttl", "\"soon\""),
            "logs.credential_ttl: must be a positive duration such as 600s, 10m or 1h".to_owned(),
        ),
        (
            good.clone().set("max_follow", "0"),
            "logs.max_follow: must be from 1 to 4096".to_owned(),
        ),
        (
            good.clone().set("max_follow", "4097"),
            "logs.max_follow: must be from 1 to 4096".to_owned(),
        ),
        (
            good.clone().set("colour", "\"blue\""),
            "logs.colour: unknown field; known: sink, url, region, endpoint, path_style, read_key_file, credentials, sts_endpoint, role_arn, mint_key_file, signing_key_file, credential_ttl, max_follow".to_owned(),
        ),
    ];
    for prefix in [
        "a/../b", "a/./b", "..", ".", "a//b", "/a", "a/", "a*", "a?", "a$", "a{b}", "a b",
    ] {
        let url = format!("\"s3://logs/{prefix}\"");
        cases.push((good.clone().set("url", &url), prefix_must.to_owned()));
    }
    for (logs, said) in &cases {
        assert_eq!(&refused(&scratch, logs), said, "{}", logs.text());
    }

    let loose = scratch.write("loose-key", &key_json("ID", "secret"), 0o640);
    let said = refused(
        &scratch,
        &good.clone().set("read_key_file", &quoted(&loose)),
    );
    let head = format!(
        "logs.read_key_file: {} must be a regular file",
        loose.display()
    );
    assert!(said.starts_with(&head), "{said}");
    let missing = scratch.path("no-such-key");
    let said = refused(
        &scratch,
        &good.clone().set("mint_key_file", &quoted(&missing)),
    );
    assert!(
        said.starts_with(&format!("logs.mint_key_file: {}: ", missing.display())),
        "{said}"
    );
    let must = "must hold JSON {\"access_key_id\": ..., \"secret_access_key\": ...} and nothing else: an id of 1 to 128 letters and digits and a secret of 1 to 1024 printable ASCII bytes";
    let bad_keys = [
        "not json".to_owned(),
        "[\"ID\", \"secret\"]".to_owned(),
        "{\"access_key_id\": \"ID\"}".to_owned(),
        "{\"access_key_id\": \"ID\", \"other\": \"x\"}".to_owned(),
        "{\"secret_access_key\": \"secret\", \"other\": 1}".to_owned(),
        "{\"access_key_id\": \"ID\", \"secret_access_key\": \"secret\", \"other\": 1}".to_owned(),
        "{\"access_key_id\": 7, \"secret_access_key\": \"secret\"}".to_owned(),
        "{\"access_key_id\": \"ID\", \"secret_access_key\": 7}".to_owned(),
        key_json("", "secret"),
        key_json(&"I".repeat(129), "secret"),
        key_json("ID/1", "secret"),
        key_json("ID", ""),
        key_json("ID", &"s".repeat(1025)),
        key_json("ID", "two words"),
    ];
    for (index, text) in bad_keys.iter().enumerate() {
        let path = scratch.write(&format!("bad-key-{index}"), text, 0o600);
        for key in ["read_key_file", "mint_key_file"] {
            let said = refused(&scratch, &good.clone().set(key, &quoted(&path)));
            assert_eq!(
                said,
                format!("logs.{key}: {} {must}", path.display()),
                "{text}"
            );
        }
    }
}

/// Asserts what loads: the design's example (`docs/design/configuration.md`) with its two key
/// files written; a table with only the required keys, whose defaults are the STS at
/// `endpoint`, virtual-host style, a TTL of one hour and 256 follows; an STS endpoint given
/// apart; the bounds of the TTL
/// (15 minutes, 12 hours), of `max_follow` (1, 4096) and of a key file (an id of 128 bytes, a
/// secret of 1024); a bucket with no prefix; and a prefix kept as written with its trailing
/// `/` (`/a//b/` is refused, `a/b` kept). It catches a default that differs from the design,
/// a bound that is off by one and a prefix that is rewritten instead of refused.
#[test]
fn a_good_logs_table_loads_with_its_defaults() {
    let scratch = Scratch::new("logs_good");
    let read = scratch.write("example-read", &key_json("EXAMPLEREAD", READ_SECRET), 0o600);
    let mint = scratch.write("example-mint", &key_json("EXAMPLEMINT", MINT_SECRET), 0o600);
    let config = loaded(&scratch, &design_example(&read, &mint)).unwrap();
    assert_eq!(config.logs().sink(), "object_store");
    let example = sink(&scratch, &design_example(&read, &mint));
    assert_eq!(example.bucket.as_str(), "bucket");
    assert_eq!(example.prefix.as_str(), "prefix/");
    assert_eq!(example.endpoint.region(), "us-east-1");
    assert_eq!(example.endpoint.host(), "bucket.objects.example");
    assert!(!example.endpoint.path_style());
    assert_eq!(example.read_key.access_key_id(), "EXAMPLEREAD");
    assert_eq!(example.read_key.secret_access_key(), READ_SECRET);
    assert_eq!(
        sts(&example),
        (
            "https://objects.example/".to_owned(),
            "arn:aws:iam::<account-id>:role/js-logs".to_owned(),
            "EXAMPLEMINT".to_owned()
        )
    );
    assert_eq!(example.credential_ttl, Duration::from_secs(3600));
    assert_eq!(example.max_follow, 256);

    let good = Logs::good(&scratch);
    let least = sink(&scratch, &good.text());
    assert_eq!(least.bucket.as_str(), "logs");
    assert_eq!(least.prefix.as_str(), "js/live/");
    assert_eq!(least.endpoint.host(), "logs.objects.example");
    assert_eq!(least.endpoint.url("/"), "https://logs.objects.example/");
    assert_eq!(
        sts(&least),
        (
            "https://objects.example/".to_owned(),
            "arn:aws:iam::example:role/js-logs".to_owned(),
            "MINTKEYID".to_owned()
        )
    );
    let Minting::Sts { key, .. } = &least.minting else {
        panic!("the sink mints with sts");
    };
    assert_eq!(key.secret_access_key(), MINT_SECRET);
    assert_eq!(least.credential_ttl, Duration::from_secs(3600));
    assert_eq!(least.max_follow, 256);

    let local = good
        .clone()
        .set("endpoint", "\"http://127.0.0.1:9000\"")
        .set("path_style", "true");
    let local = sink(&scratch, &local.text());
    let bucket = local.endpoint.bucket_path();
    assert_eq!(local.endpoint.url(&bucket), "http://127.0.0.1:9000/logs");
    assert_eq!(sts(&local).0, "http://127.0.0.1:9000/");
    let apart = good
        .clone()
        .set("endpoint", "\"https://objects.example\"")
        .set("sts_endpoint", "\"https://sts.example:8443\"")
        .set("credentials", "\"sts\"");
    assert_eq!(
        sts(&sink(&scratch, &apart.text())).0,
        "https://sts.example:8443/"
    );

    for (ttl, seconds) in [("\"15m\"", 900), ("\"12h\"", 43_200)] {
        let bound = good.clone().set("credential_ttl", ttl);
        assert_eq!(
            sink(&scratch, &bound.text()).credential_ttl,
            Duration::from_secs(seconds)
        );
    }
    for follow in [1, 4096] {
        let bound = good.clone().set("max_follow", &follow.to_string());
        assert_eq!(sink(&scratch, &bound.text()).max_follow, follow);
    }
    let longest = key_json(&"I".repeat(128), &"~".repeat(1024));
    let longest = scratch.write("longest-key", &longest, 0o600);
    let bound = good.clone().set("read_key_file", &quoted(&longest));
    assert_eq!(
        sink(&scratch, &bound.text()).read_key.access_key_id(),
        "I".repeat(128)
    );

    for (url, bucket, prefix) in [
        ("s3://logs", "logs", ""),
        ("s3://logs/a/b", "logs", "a/b/"),
        ("s3://l.o-g/A.z_0-9", "l.o-g", "A.z_0-9/"),
    ] {
        let given = good.clone().set("url", &format!("\"{url}\""));
        let given = sink(&scratch, &given.text());
        assert_eq!(
            (given.bucket.as_str(), given.prefix.as_str()),
            (bucket, prefix)
        );
    }
    let slashes = good.clone().set("url", "\"s3://logs//a//b/\"");
    assert!(refused(&scratch, &slashes).starts_with("logs.url: the log prefix must be"));
}

/// Asserts the two minting kinds: a table with no `credentials` key and one with
/// `credentials = "sts"` both load as [`Minting::Sts`]; a table with
/// `credentials = "presigned_prefix"`, no STS key and a `signing_key_file` loads as
/// [`Minting::PresignedPrefix`] holding that file's key, with the same TTL and follow defaults;
/// the longest prefix that leaves room for a host's prefix (476 bytes) loads under either kind;
/// and the `Debug` of the presigned sink names the signing key id and never its secret. It
/// catches a default kind that is not `sts`, a signing key read from the wrong file, a bound off
/// by one, and a signing secret printed.
#[test]
fn credentials_defaults_to_sts_and_presigned_prefix_loads_its_signing_key() {
    let scratch = Scratch::new("logs_kinds");
    let good = Logs::good(&scratch);
    for table in [good.clone(), good.clone().set("credentials", "\"sts\"")] {
        let loaded = sink(&scratch, &table.text());
        assert!(matches!(loaded.minting, Minting::Sts { .. }), "{loaded:?}");
    }
    let signing = scratch.write("signing-key", &key_json("SIGNKEYID", SIGNING_SECRET), 0o600);
    let presigned = good
        .clone()
        .without("role_arn")
        .without("mint_key_file")
        .set("credentials", "\"presigned_prefix\"")
        .set("signing_key_file", &quoted(&signing));
    let loaded = sink(&scratch, &presigned.text());
    let Minting::PresignedPrefix { key } = &loaded.minting else {
        panic!("the sink mints prefix grants: {loaded:?}");
    };
    assert_eq!(key.access_key_id(), "SIGNKEYID");
    assert_eq!(key.secret_access_key(), SIGNING_SECRET);
    assert_eq!(loaded.prefix.as_str(), "js/live/");
    assert_eq!(loaded.credential_ttl, Duration::from_secs(3600));
    assert_eq!(loaded.max_follow, 256);
    let shown = format!("{loaded:?}");
    assert!(shown.contains("SIGNKEYID"), "{shown}");
    assert!(!shown.contains(SIGNING_SECRET), "{shown}");

    let longest = format!("\"s3://logs/{}\"", "p".repeat(476));
    for table in [good, presigned] {
        let loaded = sink(&scratch, &table.set("url", &longest).text());
        assert_eq!(loaded.prefix.as_str().len(), 477);
    }
}

/// Asserts that no secret of the key files is printed: the `Debug` of a loaded sink shows both
/// key ids and neither secret; and the text of every refusal of a key file that holds its
/// secret where it does not belong (beside a third member, with a space in it, without the id,
/// as plain text, as the id itself) is exactly the reason with the path, and so names no
/// secret. It catches a `Debug` derived over the secret and a reason that quotes the file.
#[test]
fn no_secret_is_printed_from_the_configuration() {
    let scratch = Scratch::new("logs_secrets");
    let good = Logs::good(&scratch);
    let config = loaded(&scratch, &good.text()).unwrap();
    for shown in [
        format!("{:?}", config.logs()),
        format!("{:#?}", config.logs()),
    ] {
        assert!(shown.contains("READKEYID"), "{shown}");
        assert!(shown.contains("MINTKEYID"), "{shown}");
        assert!(!shown.contains(READ_SECRET), "{shown}");
        assert!(!shown.contains(MINT_SECRET), "{shown}");
    }

    let secret = "SecretInTheWrongPlace";
    let leaks = [
        format!(
            "{{\"access_key_id\": \"ID\", \"secret_access_key\": \"{secret}\", \"x\": \"{secret}\"}}"
        ),
        key_json("ID", &format!("{secret} {secret}")),
        format!("{{\"secret_access_key\": \"{secret}\"}}"),
        secret.to_owned(),
        format!("{{\"access_key_id\": \"{secret}/\", \"secret_access_key\": \"{secret}\"}}"),
        format!("{{\"access_key_id\": \"ID\", \"secret_access_key\": \"{secret}\"\n"),
    ];
    for (index, text) in leaks.iter().enumerate() {
        let path = scratch.write(&format!("leak-{index}"), text, 0o600);
        for key in ["read_key_file", "mint_key_file"] {
            let said = refused(&scratch, &good.clone().set(key, &quoted(&path)));
            assert!(!said.contains(secret), "{said}");
            let reason = said.strip_prefix(&format!("logs.{key}: {} ", path.display()));
            assert!(
                reason.is_some_and(|reason| reason.starts_with("must hold JSON")),
                "{said}"
            );
        }
    }
    let loose = scratch.write("loose", &key_json("ID", secret), 0o644);
    let said = refused(
        &scratch,
        &good.clone().set("read_key_file", &quoted(&loose)),
    );
    assert!(!said.contains(secret), "{said}");
}

/// What `child` wrote to standard error until it is killed. The start log line is written
/// before the address is printed, so it is there once the address is: the program is killed
/// rather than stopped with SIGTERM, which could arrive before its handler is set and is not
/// what this test is about.
fn stderr_after_kill(child: &mut Guard) -> String {
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let mut log = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut log)
        .unwrap();
    log
}

/// The standard error of the program run on `args` (the program name first), once it printed
/// its address.
fn start_log(args: &[OsString]) -> String {
    let mut child = Guard(
        Command::new(PROGRAM)
            .args(&args[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    let stdout = child.0.stdout.take().unwrap();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    assert!(line.starts_with("listening on "), "{line:?}");
    stderr_after_kill(&mut child)
}

/// Asserts the start log line of the real program: run from flags it says `logs=none`, and run
/// on a file with an object-store sink it says `logs=object_store`, and in neither does the log
/// hold a key id or a secret of the key files. It catches a start that hides the sink it runs
/// with, and a key printed at start.
#[test]
fn the_start_log_line_names_the_sink() {
    let scratch = Scratch::new("logs_start_flags");
    let (mut child, _addr) = launch(&base_args(&scratch), Stdio::piped());
    let log = stderr_after_kill(&mut child);
    let listening = log.lines().find(|line| line.contains("listening"));
    assert!(
        listening.is_some_and(|line| line.contains(" logs=none")),
        "{log}"
    );

    let scratch = Scratch::new("logs_start_file");
    let logs = Logs::good(&scratch).text();
    let pool = PoolText::static_pool(&scratch, "hosts");
    let server = server(&scratch, "listen = \"127.0.0.1:0\"\ntick = \"1h\"");
    let text = format!("{}\n{logs}", file(&server, &[&pool]));
    let path = scratch.write("js.toml", &text, 0o644);
    let args: [OsString; 3] = [
        "js-lease-server".into(),
        "--config".into(),
        path.into_os_string(),
    ];
    let log = start_log(&args);
    let listening = log.lines().find(|line| line.contains("listening"));
    assert!(
        listening.is_some_and(|line| line.contains(" logs=object_store")),
        "{log}"
    );
    for secret in ["READKEYID", "MINTKEYID", READ_SECRET, MINT_SECRET] {
        assert!(!log.contains(secret), "{log}");
    }
}
