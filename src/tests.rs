//! Unit tests: Python-generated oracle vectors (conformance/vectors, from tools/gen_vectors.py)
//! and the 2.3.0 tests that call the Python API in-process.

use crate::ojson::{self, Obj, OV};
use crate::{hash, inventory, pathx, pyerr, pytext, pyio, scan, timeparse, xml};

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/vectors/");

fn vectors(name: &str) -> Vec<Obj> {
    let text = std::fs::read_to_string(format!("{}{}", VECTORS, name)).expect("vector file (run tools/gen_vectors.py)");
    match ojson::loads(&text).unwrap() {
        OV::Arr(items) => items.into_iter().filter_map(|v| v.as_obj().cloned()).collect(),
        _ => panic!("vector file is not a list"),
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn text(hex: &str) -> String {
    ojson::decode_utf8_surrogatepass(&unhex(hex)).unwrap()
}

fn report(name: &str, failures: Vec<String>, total: usize) {
    let log = format!("{}/target/vector-failures-{}.txt", env!("CARGO_MANIFEST_DIR"), name);
    let _ = std::fs::write(&log, failures.join("\n"));
    if !failures.is_empty() {
        let shown: Vec<&String> = failures.iter().take(12).collect();
        panic!("{}: {} of {} differ from Python:\n{:#?}", name, failures.len(), total, shown);
    }
}

#[test]
fn json_str_vectors() {
    let v = vectors("json_str.json");
    let mut bad = Vec::new();
    for case in &v {
        let input = text(case.str_of("input").unwrap());
        let got = match ojson::loads(&input) {
            Ok(value) => (Some(ojson::dumps(&value, true)), None),
            Err(e) => (None, Some(e)),
        };
        let want = (case.str_of("ok").map(str::to_string), case.str_of("error").map(str::to_string));
        if got != want {
            bad.push(format!("{:?}: got {:?} want {:?}", input, got, want));
        }
    }
    report("json_str", bad, v.len());
}

#[test]
fn json_bytes_vectors() {
    let v = vectors("json_bytes.json");
    let mut bad = Vec::new();
    for case in &v {
        let input = unhex(case.str_of("input").unwrap());
        let got = ojson::loads_bytes(&input).ok().map(|value| ojson::dumps(&value, true));
        let want = case.str_of("ok").map(str::to_string);
        if got != want {
            bad.push(format!("{:?}: got {:?} want {:?}", String::from_utf8_lossy(&input), got, want));
        }
    }
    report("json_bytes", bad, v.len());
}

#[test]
fn float_vectors() {
    let v = vectors("floats.json");
    let mut bad = Vec::new();
    for case in &v {
        let bits = match case.get("bits").unwrap() {
            OV::Int(i) => *i as u64,
            OV::UInt(u) => *u,
            other => panic!("bits {:?}", other),
        };
        let f = f64::from_bits(bits);
        let got = (ojson::py_float_repr(f, false), ojson::dumps(&OV::Float(f), false));
        let want = (case.str_of("repr").unwrap().to_string(), case.str_of("json").unwrap().to_string());
        if got != want {
            bad.push(format!("{:?}: got {:?} want {:?}", f, got, want));
        }
    }
    report("floats", bad, v.len());
}

#[test]
fn time_key_vectors() {
    let v = vectors("time_key.json");
    let mut bad = Vec::new();
    for case in &v {
        let input = text(case.str_of("input").unwrap());
        let got = ojson::py_float_repr(timeparse::time_key(Some(&OV::Str(input.clone()))), false);
        let want = case.str_of("key").unwrap();
        if got != want {
            bad.push(format!("{:?}: got {} want {}", input, got, want));
        }
    }
    report("time_key", bad, v.len());
}

#[test]
fn excerpt_vectors() {
    let v = vectors("excerpt.json");
    let mut bad = Vec::new();
    for case in &v {
        let t = text(case.str_of("text").unwrap());
        let q = text(case.str_of("query").unwrap());
        let hit = Obj::new().with("text", OV::Str(t.clone())).with("_private", OV::Int(1)).with("id", OV::str("x"));
        let out = pytext::excerpt(&hit, &q);
        let got = (out.str_of("text").unwrap().to_string(), out.get("text_truncated").cloned(), out.int_of("text_start_character"));
        let want = (text(case.str_of("out_text").unwrap()), case.get("truncated").cloned(), case.int_of("start"));
        if got != want || out.contains("_private") {
            bad.push(format!("query {:?}: got {:?} want {:?}", q, (got.1, got.2), (want.1, want.2)));
        }
    }
    report("excerpt", bad, v.len());
}

#[test]
fn pathlib_vectors() {
    let name = if cfg!(windows) { "pathlib_windows.json" } else { "pathlib_posix.json" };
    let path = format!("{}{}", VECTORS, name);
    if !std::path::Path::new(&path).exists() {
        return; // generated on the platform it describes
    }
    let v = vectors(name);
    let mut bad = Vec::new();
    for case in &v {
        let s = case.str_of("s").unwrap();
        let t = case.str_of("t").unwrap();
        let parts: Vec<String> = case.get("parts").unwrap().as_arr().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
        let got = (pathx::norm(s), pathx::name(s), pathx::suffix(s), pathx::stem(s), pathx::parts(s), pathx::parent(s),
                   pathx::join(s, t), pathx::is_relative_to(&pathx::norm(s), &pathx::parent(s)));
        let want = (case.str_of("str").unwrap().to_string(), case.str_of("name").unwrap().to_string(),
                    case.str_of("suffix").unwrap().to_string(), case.str_of("stem").unwrap().to_string(), parts,
                    case.str_of("parent").unwrap().to_string(), case.str_of("join").unwrap().to_string(),
                    case.get("relative_to_parent").is_some_and(OV::truthy));
        if got != want {
            bad.push(format!("{:?} / {:?}: got {:?} want {:?}", s, t, got, want));
        }
    }
    report("pathlib", bad, v.len());
}

#[test]
fn utf8_sig_vectors() {
    let v = vectors("utf8_sig.json");
    let mut bad = Vec::new();
    for case in &v {
        let input = unhex(case.str_of("input").unwrap());
        let got = match pyerr::decode_utf8_sig(&input) {
            Ok(s) => (Some(s.to_string()), None),
            Err(e) => (None, Some(e)),
        };
        let want = (case.str_of("ok").map(text), case.str_of("error").map(str::to_string));
        if got != want {
            bad.push(format!("{:02x?}: got {:?} want {:?}", input, got, want));
        }
    }
    report("utf8_sig", bad, v.len());
}

fn tree(e: &xml::Element, out: &mut Vec<OV>) {
    let mut attrs: Vec<(String, String)> = e.attrs.clone();
    attrs.sort();
    out.push(OV::Arr(vec![
        OV::Str(e.tag.clone()),
        OV::Arr(attrs.into_iter().map(|(k, v)| OV::Arr(vec![OV::Str(k), OV::Str(v)])).collect()),
        e.text.clone().map_or(OV::Null, OV::Str),
    ]));
    for c in &e.children {
        tree(c, out);
    }
}

#[test]
fn xml_vectors() {
    let v = vectors("xml.json");
    let mut bad = Vec::new();
    for case in &v {
        let input = unhex(case.str_of("input").unwrap());
        let got = match xml::fromstring(&input) {
            Ok(root) => {
                let mut t = Vec::new();
                tree(&root, &mut t);
                (Some(OV::Arr(t)), None)
            }
            Err(e) => (None, Some(format!("{}: {}", e.class, e.msg))),
        };
        let want = (case.get("tree").filter(|t| !t.is_null()).cloned(), case.str_of("error").map(str::to_string));
        if got != want {
            bad.push(format!("{:?}: got {:?} want {:?}", String::from_utf8_lossy(&input), got.1, want.1));
        }
    }
    report("xml", bad, v.len());
}

#[test]
fn hash_known_answers() {
    assert_eq!(hash::sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(hash::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let million = vec![b'a'; 1_000_000];
    assert_eq!(hash::sha256_hex(&million), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    assert_eq!(hash::crc32_update(0, b"123456789"), 0xCBF43926);
    // Streaming in odd pieces equals one pass, for both checks.
    let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 + 3) as u8).collect();
    let mut h = hash::Sha256::default();
    let mut crc = 0;
    for chunk in data.chunks(997) {
        h.update(chunk);
        crc = hash::crc32_update(crc, chunk);
    }
    assert_eq!(h.hex(), hash::sha256_hex(&data));
    assert_eq!(crc, hash::crc32_update(0, &data));
}

// ------------------------------------------------------------ in-process 2.3.0 equivalents ---

const A: &str = "11111111-1111-4111-8111-111111111111";

struct Fixture {
    root: String,
    config: Obj,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let root = format!("{}/target/unit-tmp/{}-{}", env!("CARGO_MANIFEST_DIR"), name, std::process::id());
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(format!("{}/claude/project", root)).unwrap();
        let root = pathx::norm(&root);
        let source = Obj::new().with("name", OV::str("claude")).with("kind", OV::str("claude")).with("path", OV::Str(pathx::join(&root, "claude")));
        Fixture { root, config: Obj::new().with("sources", OV::Arr(vec![OV::Obj(source)])) }
    }

    fn transcript(&self) -> String {
        pathx::join(&self.root, &format!("claude/project/{}.jsonl", A))
    }

    fn write(&self, texts: &[&str]) {
        let body: String = texts
            .iter()
            .map(|t| format!("{{\"type\": \"user\", \"sessionId\": \"{}\", \"message\": {{\"role\": \"user\", \"content\": \"{}\"}}}}\n", A, t))
            .collect();
        std::fs::write(self.transcript(), body).unwrap();
    }

    fn inventory(&self) -> (Vec<Obj>, Obj) {
        let mut cov = inventory::initial_coverage(&self.config);
        let rows = inventory::inventory(&self.config, &mut cov, true).unwrap();
        (rows, cov)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn read_all(row: &mut Obj, cov: &mut Obj, needle: Option<&str>, strong: bool) -> Vec<Obj> {
    let ids = scan::SelfIds(vec![]);
    let mut out = Vec::new();
    scan::records(row, cov, needle, strong, &ids, &mut |hit| {
        out.push(hit);
        true
    })
    .unwrap();
    out
}

#[test]
fn changed_between_inventory_and_read_is_partial() {
    let f = Fixture::new("changed");
    f.write(&["needle"]);
    let (mut rows, mut cov) = f.inventory();
    f.write(&["needle changed after inventory"]);
    read_all(&mut rows[0], &mut cov, None, true);
    assert!(!inventory::is_complete(&cov));
    let errors = cov.get("errors").and_then(OV::as_arr).unwrap();
    assert!(errors.iter().any(|e| e.as_obj().and_then(|e| e.str_of("kind")) == Some("changed_since_inventory")));
}

#[test]
fn append_after_inventory_preserves_completed_prefix() {
    let f = Fixture::new("append");
    f.write(&["needle original"]);
    let (mut rows, mut cov) = f.inventory();
    let mut file = std::fs::OpenOptions::new().append(true).open(f.transcript()).unwrap();
    use std::io::Write;
    writeln!(file, "{{\"type\": \"user\", \"sessionId\": \"{}\", \"message\": {{\"role\": \"user\", \"content\": \"later\"}}}}", A).unwrap();
    drop(file);
    let records = read_all(&mut rows[0], &mut cov, None, true);
    assert!(inventory::is_complete(&cov), "{:?}", cov);
    let texts: Vec<&str> = records.iter().map(|r| r.str_of("text").unwrap()).collect();
    assert_eq!(texts, ["needle original"]);
}

#[test]
fn prefix_check_crc_detects_in_place_change() {
    let f = Fixture::new("prefix");
    f.write(&["stable text"]);
    let p = f.transcript();
    let size = std::fs::metadata(&p).unwrap().len();
    let crc = scan::prefix_check(&p, size, false).unwrap();
    assert!(crc.starts_with("crc32:"));
    assert_eq!(scan::prefix_check(&p, size, true).unwrap(), hash::sha256_hex(&std::fs::read(&p).unwrap()));
    let mut data = std::fs::read(&p).unwrap();
    let at = data.windows(6).position(|w| w == b"stable").unwrap();
    data[at] = b'S';
    std::fs::write(&p, &data).unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len(), size);
    assert_ne!(scan::prefix_check(&p, size, false).unwrap(), crc);
}

#[test]
fn only_strong_reads_pin_the_prefix() {
    let f = Fixture::new("strong");
    f.write(&["needle"]);
    let (rows, _) = f.inventory();
    let (mut crc_row, mut sha_row) = (rows[0].clone(), rows[0].clone());
    let mut cov = inventory::initial_coverage(&f.config);
    read_all(&mut crc_row, &mut cov, Some("needle"), false);
    read_all(&mut sha_row, &mut cov, Some("needle"), true);
    assert!(!crc_row.contains("_prefix_sha256"));
    let size = sha_row.int_of("bytes").unwrap() as u64;
    let path = sha_row.str_of("path").unwrap().to_string();
    assert_eq!(sha_row.str_of("_prefix_sha256").unwrap(), scan::prefix_check(&path, size, true).unwrap());
}

#[test]
fn guards_equal_capture() {
    let f = Fixture::new("guards");
    f.write(&[&"x".repeat(9000), "needle"]);
    let (rows, _) = f.inventory();
    for row in rows {
        let path = row.str_of("path").unwrap();
        let size = row.int_of("bytes").unwrap() as u64;
        assert_eq!(row.get("_guards").unwrap(), &pyio::capture_guards(path, size).unwrap());
    }
}

/// (role, text, block) of every field `row_blocks` visits in one Antigravity step.
fn antigravity_fields(step: &str) -> Vec<(String, String, String)> {
    use crate::fields::{row_blocks, Role};
    let event = ojson::loads(step).unwrap();
    let mut out = Vec::new();
    row_blocks(true, &event, true, &mut |role, text, block| {
        let role = match role {
            Role::Name(n) => n.to_string(),
            Role::Value(v) => ojson::dumps(v, false),
        };
        let block = match block.to_ov() {
            OV::Str(s) => s,
            other => ojson::dumps(&other, false),
        };
        out.push((role, text.to_string(), block));
    })
    .unwrap();
    out
}

fn fields_of(items: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
    items.iter().map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())).collect()
}

#[test]
fn antigravity_user_request_spans() {
    use crate::fields::user_request_spans;
    assert_eq!(user_request_spans("<USER_REQUEST>\nask\n</USER_REQUEST>\n<ADDITIONAL_METADATA>t</ADDITIONAL_METADATA>"),
               vec![(true, "\nask\n"), (false, "\n<ADDITIONAL_METADATA>t</ADDITIONAL_METADATA>")]);
    assert_eq!(user_request_spans("lead <USER_REQUEST>open"), vec![(false, "lead "), (true, "open")]);
    assert_eq!(user_request_spans("a<USER_REQUEST>b</USER_REQUEST>c<USER_REQUEST>d</USER_REQUEST>"),
               vec![(false, "a"), (true, "b"), (false, "c"), (true, "d")]);
    assert_eq!(user_request_spans("no tags"), vec![(false, "no tags")]);
    assert!(user_request_spans("<USER_REQUEST></USER_REQUEST>").is_empty());
}

#[test]
fn antigravity_operator_text_is_only_inside_user_request() {
    let explicit = r#"{"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nask\n</USER_REQUEST>\n<ADDITIONAL_METADATA>m</ADDITIONAL_METADATA>"}"#;
    assert_eq!(antigravity_fields(explicit),
               fields_of(&[("user", "\nask\n", "0"), ("system", "\n<ADDITIONAL_METADATA>m</ADDITIONAL_METADATA>", "1")]));
    let implicit = r#"{"type":"USER_INPUT","source":"USER_IMPLICIT","content":"<USER_REQUEST>ask</USER_REQUEST>"}"#;
    assert_eq!(antigravity_fields(implicit), fields_of(&[("system", "ask", "0")]));
    let steps = r#"[{"type":"CHECKPOINT","content":"c"},{"type":"EPHEMERAL_MESSAGE","content":"e"},{"type":"GENERIC","content":"g"},
                   {"type":"ERROR_MESSAGE","error":"x"},{"type":"NEW_KIND","content":"n"},{"type":"GENERIC"}]"#;
    let OV::Arr(items) = ojson::loads(steps).unwrap() else { panic!() };
    let got: Vec<_> = items.iter().flat_map(|s| antigravity_fields(&ojson::dumps(s, false))).collect();
    assert_eq!(got, fields_of(&[("summary", "c", "0"), ("system", "e", "0"), ("tool", "g", "0"), ("system", "x", "[\"error\"]"),
                                ("unknown", "n", "0")]));
}

#[test]
fn antigravity_thinking_is_never_visited() {
    let step = r#"{"type":"PLANNER_RESPONSE","source":"MODEL","content":"visible","thinking":"hidden plan",
                   "tool_calls":[{"name":"run_command","args":{"CommandLine":"\"dir N:\\\\\"","WaitMsBeforeAsync":"5000",
                   "Edits":"[{\"Target\":\"t\"}]","Content":"\"cut\n<truncated 9 bytes>\n\""}}]}"#;
    let got = antigravity_fields(step);
    assert_eq!(got, fields_of(&[
        ("assistant", "visible", "0"),
        ("tool_input", "dir N:\\", "[\"tool_calls\",0,\"args\",\"CommandLine\"]"),
        ("tool_input", "t", "[\"tool_calls\",0,\"args\",\"Edits\",0,\"Target\"]"),
        ("tool_input", "\"cut\n<truncated 9 bytes>\n\"", "[\"tool_calls\",0,\"args\",\"Content\"]"),
    ]));
    assert!(got.iter().all(|(_, text, _)| !text.contains("hidden")));
    let silent = r#"{"type":"PLANNER_RESPONSE","source":"MODEL","thinking":"hidden plan"}"#;
    assert!(antigravity_fields(silent).is_empty());
}
