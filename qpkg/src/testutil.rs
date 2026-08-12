//! Builders for synthetic pacman/AUR payloads. Tests construct every archive
//! in memory with the same crates the parsers read them with — no binary
//! fixtures in git.

use std::io::{Read, Write};

/// A fetched body for stubbing the `sources::Fetch` contract.
pub fn body(bytes: &[u8]) -> Box<dyn Read + Send> {
    Box::new(std::io::Cursor::new(bytes.to_vec()))
}

pub fn desc(fields: &[(&str, &[&str])]) -> String {
    let mut out = String::new();
    for (key, values) in fields {
        out.push_str(&format!("%{key}%\n"));
        for v in *values {
            out.push_str(v);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

pub fn tar_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, content.as_bytes()).unwrap();
    }
    builder.into_inner().unwrap()
}

pub fn gzipped(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

pub fn zstded(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, 1).unwrap()
}

/// A one-package repository database, gzipped.
pub fn mini_db(name: &str, version: &str) -> Vec<u8> {
    let entry = desc(&[
        ("NAME", &[name]),
        ("BASE", &[name]),
        ("VERSION", &[version]),
        ("DESC", &[&format!("the {name} package")]),
    ]);
    gzipped(&tar_bytes(&[(&format!("{name}-{version}/desc"), &entry)]))
}

/// An AUR metadata dump with the given (name, version) pairs, gzipped.
pub fn mini_aur(entries: &[(&str, &str)]) -> Vec<u8> {
    let body: Vec<String> = entries
        .iter()
        .map(|(name, version)| {
            format!(
                r#"{{"Name":"{name}","PackageBase":"{name}","Version":"{version}","Description":"{name} from the AUR"}}"#
            )
        })
        .collect();
    gzipped(format!("[{}]", body.join(",")).as_bytes())
}
