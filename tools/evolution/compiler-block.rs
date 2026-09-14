//! Browser compiler Block: pure bytes-to-bytes partial evaluation over StructFS.
//! This file is copied beside the pinned evaluator sources by prepare-compiler.
#![allow(dead_code)]
mod artifact;
mod cache;
mod constant_offsets;
mod dce;
mod directive;
mod escape;
mod eval;
mod image;
mod intrinsics;
mod liveness;
mod state;
mod stats;
mod value;

use base64::{engine::general_purpose::STANDARD, Engine};
use featherweight_guest::sdk;
use serde_json::{json, Value};

struct Diagnostics;
static DIAGNOSTICS: Diagnostics = Diagnostics;
impl log::Log for Diagnostics {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            let _ = sdk::write_typed(
                "iso/log/error",
                &serde_json::to_vec(&r.args().to_string()).unwrap(),
            );
        }
    }
    fn flush(&self) {}
}

// Progress is disabled: no clock, filesystem, threads, or native compiler host.
struct Progress;
impl Progress {
    fn set_length(&self, _: u64) {}
    fn inc(&self, _: u64) {}
    fn tick(&self) {}
    fn finish_and_clear(&self) {}
}

fn compile(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut options = waffle::FrontendOptions::default();
    options.debug = true;
    let module = waffle::Module::from_wasm_bytes(bytes, &options)?;
    let mut memory = image::build_image(&module, None)?;
    let directives = directive::collect(&module, &mut memory)?;
    anyhow::ensure!(!directives.is_empty(), "no hot traces to specialize");
    let mut result =
        eval::partially_evaluate(module, &mut memory, &directives, None, None, &cache::Cache)?;
    image::update(&mut result.module, &memory);
    // Runtime normalization removes annotation imports separately, replacing
    // residual intrinsic calls with traps rather than silently wrong stubs.
    artifact::executable(&result.module.to_wasm_bytes()?)
}

#[no_mangle]
pub unsafe extern "C" fn manifest(ret: *mut sdk::Ret) -> i32 {
    let bytes =
        br#"{"name":"zaqaru-optimizer","version":"0.1.0","serialization":"application/json"}"#;
    let ptr = sdk::block_alloc(bytes.len() as i32);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        (*ret).ptr = ptr as u32;
        (*ret).len = bytes.len() as u32;
    }
    0
}

fn serve() -> anyhow::Result<()> {
    let mut outputs = Vec::new();
    while let Some(bytes) = sdk::read_typed("iso/server/requests")? {
        let request: Value = serde_json::from_slice(&bytes)?;
        if request.is_null() {
            break;
        }
        let respond_to = request["respond_to"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing reply path"))?;
        let result = (|| -> anyhow::Result<Value> {
            if request["op"] == "read" {
                let path = request["path"].as_str().unwrap_or("");
                let id: usize = path
                    .strip_prefix("results/")
                    .ok_or_else(|| anyhow::anyhow!("unknown path"))?
                    .parse()?;
                return Ok(match outputs.get(id) {
                    Some(value) => json!({"result":"ok", "present":true, "value":value}),
                    None => json!({"result":"ok", "present":false}),
                });
            }
            anyhow::ensure!(request["op"] == "write", "unsupported request");
            let encoded = request["data"]["wasm"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing wasm"))?;
            let input = STANDARD.decode(encoded)?;
            let output = match request["path"].as_str() {
                Some("executable") => artifact::executable(&input)?,
                Some("compile") => {
                    let input = if let Some(length) = request["data"]["memoryLength"].as_u64() {
                        anyhow::ensure!(
                            length <= 1024 * 1024 * 1024 && length % 65536 == 0,
                            "unsupported memory size"
                        );
                        let mut memory = vec![0; length as usize];
                        let pages = request["data"]["pages"]
                            .as_array()
                            .ok_or_else(|| anyhow::anyhow!("missing memory pages"))?;
                        let mut last_end = 0;
                        for page in pages {
                            let offset = page[0]
                                .as_u64()
                                .ok_or_else(|| anyhow::anyhow!("bad page offset"))?
                                as usize;
                            let bytes = STANDARD.decode(
                                page[1]
                                    .as_str()
                                    .ok_or_else(|| anyhow::anyhow!("bad page bytes"))?,
                            )?;
                            anyhow::ensure!(
                                offset >= last_end
                                    && offset <= memory.len()
                                    && bytes.len() <= memory.len() - offset,
                                "overlapping or out-of-bounds page"
                            );
                            memory[offset..offset + bytes.len()].copy_from_slice(&bytes);
                            last_end = offset + bytes.len();
                        }
                        let stack = i32::try_from(
                            request["data"]["stackPointer"]
                                .as_i64()
                                .ok_or_else(|| anyhow::anyhow!("missing stack pointer"))?,
                        )?;
                        artifact::checkpoint(&input, &memory, stack)?
                    } else {
                        input
                    };
                    compile(&input)?
                }
                _ => anyhow::bail!("unsupported request path"),
            };
            let id = outputs.len();
            outputs.push(json!({"wasm": STANDARD.encode(output)}));
            Ok(json!({"result":"ok", "path": format!("results/{id}")}))
        })();
        let response = match result {
            Ok(value) => value,
            Err(error) => {
                json!({"result":"error", "error":{"type":"store_error", "message":error.to_string(), "retryable":false}})
            }
        };
        sdk::write_typed(respond_to, &serde_json::to_vec(&response)?)?;
    }
    sdk::write_typed("iso/shutdown/complete", b"null")?;
    Ok(())
}

#[no_mangle]
pub extern "C" fn run() -> i32 {
    std::panic::set_hook(Box::new(|info| {
        let _ = sdk::write_typed("iso/log/error", &serde_json::to_vec(&info.to_string()).unwrap());
    }));
    let _ = log::set_logger(&DIAGNOSTICS);
    log::set_max_level(log::LevelFilter::Warn);
    match serve() {
        Ok(()) => 0,
        Err(error) => {
            let _ = sdk::write_typed(
                "iso/log/error",
                &serde_json::to_vec(&error.to_string()).unwrap(),
            );
            1
        }
    }
}
