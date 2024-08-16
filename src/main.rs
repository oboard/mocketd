extern crate iron;
use anyhow::anyhow;

use iron::prelude::*;
use iron::status;

use serde_json::Value;
use serde_json::json;
use std::env;
use std::fs;
use std::process;
use std::sync::{Arc, Mutex};
use wasmtime::*;

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
                            let _ = handle_receive(json_value);
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

fn send_event<T>(store: &mut Store<T>, instance: &Instance, event_type: &str, data: Value) {
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

#[tokio::main]
async fn main() {
    let wasm_path = env::args().nth(1).expect("Usage: <wasm-file>");

    // Initialize WASM and get store and instance
    let (mut store, instance) = init_wasm(&wasm_path);



    // Optionally call '_start' if it exists
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        if let Err(err) = start.call(&mut store, ()) {
            eprintln!("Failed to execute '_start': {}", err);
            process::exit(1);
        }
    } else {
        println!("No '_start' function found in {}", wasm_path);
    }

    // send_event(&mut store, &instance, "echo", json!("Hello, World!"));

    // keep the main thread alive till ctrl c is pressed
    tokio::signal::ctrl_c().await.unwrap();
    process::exit(0); 
}

// Function to handle the parsed JSON object
fn handle_receive(json_value: Value) -> std::io::Result<()> {
    println!("Received JSON: {}", json_value);

    fn listen(port: u16) -> std::io::Result<()> {
        fn handler(req: &mut Request) -> IronResult<Response> {
            println!("Received request: {}", req.url);
            Ok(Response::with((status::Ok, "Hello, World!")))
        }

        // Create the Iron server and assign the handler.
        Iron::new(handler)
            .http("0.0.0.0:".to_owned() + &port.to_string())
            .unwrap();
        Ok(())
    }

    let handle_type = json_value[0].as_str();
    let handle_data = &json_value[1];
    match handle_type {
        Some(t) => match t {
            "http.createServer" => Ok(()),
            "http.listen" => {
                let port = handle_data.as_f64();
                match port {
                    Some(port) => listen(port as u16),
                    _ => {
                        eprintln!("Invalid port value");
                        Ok(())
                    }
                }
            }
            _ => {
                println!("Unknown handle type");
                Ok(())
            }
        },
        _ => {
            println!("Invalid handle type");
            Ok(())
        }
    }
}
