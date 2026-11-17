//! Prints what we read from a GGUF header: `cargo run -p superviser-core --example gguf_info -- FILE`.

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let info = superviser_core::gguf::read_file(path.as_ref())?;
        let mut ints: Vec<_> = info.ints.iter().collect();
        ints.sort();
        let mut arrays: Vec<_> = info.int_arrays.iter().map(|(k, v)| (k, v.len(), v.iter().take(12).collect::<Vec<_>>())).collect();
        arrays.sort();
        println!("{path}\n  arch {} name {:?} type {:?} file_type {:?}\n  ints {ints:?}\n  arrays {arrays:?}\n  kv/token {:?}", info.arch, info.name, info.kind, info.file_type, info.kv_bytes_per_token());
    }
    Ok(())
}
