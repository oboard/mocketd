mod nodehttp;

// use nodehttp::Request;
// use nodehttp::Response;

use anyhow::anyhow;
use nodehttp::Response;

use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wasmtime::*;

static LOG_LEVEL: AtomicUsize = AtomicUsize::new(0);

fn set_log_level(level: usize) {
    LOG_LEVEL.store(level, Ordering::Relaxed);
}

fn log(level: usize, message: &str) {
    if level <= LOG_LEVEL.load(Ordering::Relaxed) {
        println!("{}", message);
    }
}

static mut WASM_STORE: Option<Store<()>> = None;
static mut WASM_INSTANCE: Option<Instance> = None;

#[macro_use]
extern crate lazy_static;

lazy_static! {
    static ref RESPONSE_MAP: Arc<Mutex<HashMap<usize, Response>>> =
        Arc::new(Mutex::new(HashMap::new()));
    static ref NEXT_ID: AtomicUsize = AtomicUsize::new(0);
}

// Define the function to initialize WASM and return an instance and store
fn init_wasm(wasm_path: &str) -> (Store<()>, Instance) {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);

    // Define function types
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let h_sd_ty = FuncType::new(&engine, vec![ValType::I32], vec![]);
    let h_se_ty = FuncType::new(&engine, vec![], vec![]);
    let print_char_ty = FuncType::new(&engine, vec![ValType::I32], vec![]);

    // Define h_sd function
    let buffer_for_h_sd = Arc::clone(&buffer);
    linker
        .func_new("__h", "h_sd", h_sd_ty, move |_, params: &[Val], _| {
            if let [Val::I32(ch)] = params {
                buffer_for_h_sd.lock().unwrap().push(*ch as u16);
            }
            Ok(())
        })
        .unwrap();

    // Define h_se function
    let buffer_for_h_se = Arc::clone(&buffer);
    linker
        .func_new("__h", "h_se", h_se_ty, move |_, _, _| {
            let mut data = buffer_for_h_se.lock().unwrap();
            if !data.is_empty() {
                if let Ok(utf8_string) = String::from_utf16(&data) {
                    let clean_string = utf8_string.replace("\0", "");
                    if let Ok(json_value) = serde_json::from_str::<Value>(&clean_string) {
                        tokio::spawn(async move {
                            handle_receive(json_value).await.unwrap();
                        });
                    } else {
                        eprintln!("Failed to parse JSON.");
                        println!("{}", clean_string);
                    }
                }
                // Clear the buffer after processing
                data.clear();
            }
            Ok(())
        })
        .unwrap();

    // Define `spectest::print_char` function
    let print_buffer = Arc::new(Mutex::new(Vec::new()));
    linker
        .func_new(
            "spectest",
            "print_char",
            print_char_ty,
            move |_, params: &[Val], _| {
                if let [Val::I32(ch)] = params {
                    let mut buffer = print_buffer.lock().unwrap();
                    if *ch == '\n' as i32 {
                        println!("{}", String::from_utf16(&buffer).unwrap());
                        buffer.clear();
                    } else if *ch != '\r' as i32 {
                        buffer.push(*ch as u16);
                    }
                }
                Ok(())
            },
        )
        .unwrap();

    // Load and compile WASM module
    let wasm_bytes = fs::read(wasm_path).unwrap_or_else(|err| {
        eprintln!("Failed to read file {}: {}", wasm_path, err);
        process::exit(1);
    });
    let module = Module::new(&engine, &wasm_bytes).unwrap_or_else(|err| {
        eprintln!("Failed to create module: {}", err);
        process::exit(1);
    });

    // Instantiate the WASM module
    let instance = linker
        .instantiate(&mut store, &module)
        .unwrap_or_else(|err| {
            eprintln!("Failed to instantiate module: {}", err);
            process::exit(1);
        });

    (store, instance)
}

fn h_rd<T>(store: &mut Store<T>, instance: &Instance, ch: i32) -> Result<()> {
    let start_func = instance
        .get_func(store.as_context_mut(), "h_rd")
        .ok_or_else(|| anyhow!("h_rd function not found"))?;
    start_func.call(store.as_context_mut(), &[wasmtime::Val::I32(ch)], &mut [])?;

    Ok(())
}

fn h_re<T>(store: &mut Store<T>, instance: &Instance) -> Result<()> {
    let start_func = instance
        .get_func(store.as_context_mut(), "h_re")
        .ok_or_else(|| anyhow!("h_re function not found"))?;
    start_func.call(store.as_context_mut(), &[], &mut [])?;

    Ok(())
}

fn send_event(event_type: &str, data: Value) {
    let store = unsafe { WASM_STORE.as_mut() };
    let instance = unsafe { WASM_INSTANCE.as_ref() };
    match (store, instance) {
        (Some(store), Some(instance)) => {
            let json = json!([event_type, data]).to_string();
            let utf16: Vec<u16> = json.encode_utf16().collect();
            let mut uint8array = Vec::with_capacity(utf16.len() * 2);
            for &word in utf16.iter() {
                uint8array.push((word >> 8) as u8);
                uint8array.push(word as u8);
            }
            for &byte in uint8array.iter() {
                let _ = h_rd(store, instance, byte as i32);
            }
            let _ = h_re(store, instance);
        }

        _ => {
            eprintln!("WASM not initialized");
            return;
        }
    }
}

#[tokio::main]
async fn main() {
    let matches = clap::Command::new("Mocket Runtime")
        .version("1.0")
        .author("oboard <oboard@outlook.com>")
        .about("a WebAssembly runtime for Mocket")
        .arg(
            clap::Arg::new("wasm_file")
                .help("Path to the WebAssembly file")
                .required(true)
                .index(1),
        )
        .arg(
            clap::Arg::new("log_level")
                .short('l')
                .long("log")
                .help("Sets the log level (0: no logs, 1: minimal logs, 2: verbose logs)"),
        )
        .get_matches();

    let wasm_path = matches.get_one::<String>("wasm_file").unwrap();
    let log_level = (*matches
        .get_one::<String>("log_level")
        .unwrap_or(&"0".to_string()))
    .parse::<usize>()
    .unwrap_or(0);

    // Set log level (this is just an example, adapt to your logging needs)
    match log_level {
        0 => println!("Log level: 0 (No logs)"),
        1 => println!("Log level: 1 (Minimal logs)"),
        2 => println!("Log level: 2 (Verbose logs)"),
        _ => println!("Unknown log level: {}", log_level),
    }

    set_log_level(log_level);

    // Initialize WASM and get store and instance
    let (store, instance) = init_wasm(&wasm_path);
    unsafe {
        WASM_STORE = Some(store);
        WASM_INSTANCE = Some(instance);
    }
    // Optionally call '_start' if it exists
    let instance = unsafe { WASM_INSTANCE.as_ref().unwrap() };
    let mut store: Store<()> = unsafe { WASM_STORE.take().unwrap() };
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        if let Err(err) = start.call(&mut store, ()) {
            log(1, &format!("Failed to execute '_start': {}", err));
            process::exit(1);
        }
    } else {
        log(2, &format!("No '_start' function found in {}", wasm_path));
    }

    unsafe {
        WASM_STORE = Some(store);
        WASM_INSTANCE = Some(*instance);
    }

    // keep the main thread alive till ctrl c is pressed
    tokio::signal::ctrl_c().await.unwrap();
    process::exit(0);
}

fn map_to_iter(
    map: serde_json::Map<String, Value>,
) -> impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)> {
    map.into_iter().filter_map(|(key, value)| {
        // Try to convert the value to a string reference
        if let Value::String(s) = value {
            Some((key, s)) // Return a tuple with (key, value) where both are &str
        } else {
            None // Ignore non-string values
        }
    })
}

// Function to handle the parsed JSON object
async fn handle_receive(json_value: Value) -> std::io::Result<()> {
    log(1, &format!("Received JSON: {}", json_value));

    async fn listen(port: u16) -> std::io::Result<()> {
        log(1, &format!("Listening on port {}", port));

        let server = nodehttp::create_server(|req, mut res| {
            log(2, &format!("Received request: {} {}", req.method, req.path));

            if [
                "GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "CONNECT", "TRACE", "PATCH",
            ]
            .contains(&(req.method.as_str()))
            {
                Box::pin(async move {
                    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
                    let data = json!([
                        {
                            "method": req.method,
                            "url": req.path,
                        },
                        {
                            "id": id,
                        }
                    ]);
                    log(1, &format!("{}", data));
                    send_event("http.request", data);

                    // 存储 ID 和响应的映射
                    let mut response_map = RESPONSE_MAP.lock().unwrap();
                    response_map.insert(id, res);

                    Ok(())
                })
            } else {
                Box::pin(async move {
                    log(2, &format!("Invalid method `{}`", req.method));
                    // res.write_head(
                    //     200,
                    //     std::collections::HashMap::from([("Content-Type", "text/plain")]),
                    // )
                    // .await?;
                    res.end("").await?;
                    Ok(())
                })
            }
        });

        // 让服务器监听 3000 端口
        server.listen(port, || {}).await
    }

    let handle_type = json_value[0].as_str();
    let handle_data = &json_value[1];
    match handle_type {
        Some(t) => match t {
            "http.listen" => {
                let port = handle_data.as_f64();
                match port {
                    Some(port) => listen(port as u16).await,
                    _ => {
                        eprintln!("Invalid port value");
                        Ok(())
                    }
                }
            }
            // "http.writeHead" => {
            //     if let Value::Array(vec) = handle_data {
            //         match vec.as_slice() {
            //             [Value::Number(id), Value::Number(status_code), Value::Object(headers)] => {
            //                 let index = id.as_f64().unwrap_or(0f64) as usize;
            //                 let response = unsafe { RESPONSE_STACK.get_mut(index) };
            //                 let status_code = status_code.as_f64().unwrap_or(500f64) as u16;
            //                 // let headers = headers;
            //                 match response {
            //                     Some(response) => {
            //                         response
            //                             .write_head(
            //                                 status_code,
            //                                 HashMap::from([("Content-Type", "text/plain")]),
            //                             )
            //                             .await?;
            //                     }
            //                     None => {
            //                         eprintln!("Invalid response id");
            //                         return Ok(());
            //                     }
            //                 }

            //                 Ok(())
            //             }
            //             _ => {
            //                 eprintln!("Invalid http.writeHead data");
            //                 Ok(())
            //             }
            //         }
            //     } else {
            //         println!("Expected an array.");
            //         Ok(())
            //     }
            // }
            "http.end" => {
                if let Value::Array(vec) = handle_data {
                    match vec.as_slice() {
                        [Value::Number(id), Value::Number(status_code), Value::Object(headers), body] =>
                        {
                            let index = id.as_f64().unwrap_or(0f64) as usize;
                            let headers = headers;
                            log(3, format!("index: {}", index).as_str());
                            let mut response_map = RESPONSE_MAP.lock().unwrap();
                            let response = response_map.remove(&index);
                            match response {
                                Some(mut response) => {
                                    response
                                        .write_head(
                                            status_code.as_f64().unwrap_or(500f64) as u16,
                                            map_to_iter(headers.clone()),
                                        )
                                        .await?;

                                    // 如果是string则直接发送，如果是json object则strinify
                                    match body {
                                        Value::String(s) => {
                                            response.end(s).await?;
                                        }
                                        Value::Object(o) => {
                                            let json_string = serde_json::to_string(o).unwrap();
                                            response.end(&json_string).await?;
                                        }
                                        _ => {
                                            eprintln!("Invalid body type");
                                        }
                                    }
                                    Ok(())
                                }
                                _ => {
                                    eprintln!("Invalid response id");
                                    Ok(())
                                }
                            }
                        }
                        _ => {
                            eprintln!("Invalid http.end data");
                            Ok(())
                        }
                    }
                } else {
                    println!("Expected an array.");
                    Ok(())
                }
            }
            _ => {
                println!("Unknown method `{}`", t);
                Ok(())
            }
        },
        _ => {
            println!("Invalid handle type");
            Ok(())
        }
    }
}
