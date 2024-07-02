use std::{io::Read, sync::mpsc, thread, time::Duration};

use seda_runtime_sdk::{ExecutionResult, ExitInfo, VmCallData, VmResult, VmResultStatus};
use wasmer::Instance;
use wasmer_wasix::{Pipe, WasiEnv, WasiRuntimeError};

use crate::{context::VmContext, runtime_context::RuntimeContext, vm_imports::create_wasm_imports};

fn internal_run_vm(
    call_data: VmCallData,
    mut context: RuntimeContext,
    stdout: &mut Vec<String>,
    stderr: &mut Vec<String>,
) -> ExecutionResult<(Vec<u8>, i32)> {
    tracing::debug!("Running VM with call data: {:?}", call_data);
    // _start is the default WASI entrypoint
    let function_name = call_data.clone().start_func.unwrap_or_else(|| "_start".to_string());

    let (stdout_tx, mut stdout_rx) = Pipe::channel();
    let (stderr_tx, mut stderr_rx) = Pipe::channel();

    let (sender, receiver) = mpsc::channel();

    let dr_timeout = Duration::from_nanos(100_000_000_000);

    // An approach to handle the runtime execution having a timeout
    // we could use the tokio::time::timeout function to wrap the execution but that takes a future
    // or we could use actix to time this
    let handle = thread::spawn(move || {
        // leftovers from upgrading to wasmer 4.2.4...
        // there has to be a cleaner way to do this
        // maybe actix to spawn a future that times out???
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();

        let mut wasi_env = WasiEnv::builder(function_name.clone())
            .envs(call_data.envs.clone())
            .args(call_data.args.clone())
            .stdout(Box::new(stdout_tx))
            .stderr(Box::new(stderr_tx))
            .finalize(&mut context.wasm_store)
            .map_err(|_| VmResultStatus::WasiEnvInitializeFailure)?;

        let vm_context = VmContext::create_vm_context(&mut context.wasm_store, wasi_env.env.clone());

        let imports = create_wasm_imports(
            &mut context.wasm_store,
            &vm_context,
            &wasi_env,
            &context.wasm_module,
            &call_data,
        )
        .map_err(|_| VmResultStatus::FailedToCreateVMImports)?;

        let wasmer_instance = Instance::new(&mut context.wasm_store, &context.wasm_module, &imports)
            .map_err(|e| VmResultStatus::FailedToCreateWasmerInstance(e.to_string()))?;

        let env_mut = vm_context.as_mut(&mut context.wasm_store);
        env_mut.memory = Some(
            wasmer_instance
                .exports
                .get_memory("memory")
                .map_err(|_| VmResultStatus::FailedToGetWASMMemory)?
                .clone(),
        );

        wasi_env
            .initialize(&mut context.wasm_store, wasmer_instance.clone())
            .map_err(|_| VmResultStatus::FailedToGetWASMFn)?;

        let main_func = wasmer_instance
            .exports
            .get_function(&function_name)
            .map_err(|_| VmResultStatus::FailedToGetWASMFn)?;

        let runtime_result = main_func.call(&mut context.wasm_store, &[]);

        wasi_env.cleanup(&mut context.wasm_store, None);
        drop(_guard);

        let mut exit_code: i32 = 0;

        if let Err(err) = runtime_result {
            // we convert the error to a wasix error
            let wasix_error = WasiRuntimeError::from(err);

            if let Some(wasi_exit_code) = wasix_error.as_exit_code() {
                exit_code = wasi_exit_code.raw();
            }
        }

        let execution_result = vm_context.as_ref(&context.wasm_store).result.lock();

        if let Err(e) = sender.send(()) {
            tracing::error!("Failed to send result: {:?}", e);
        }

        Ok((execution_result.clone(), exit_code))
    });
    tracing::debug!("Waiting for VM to complete or timeout");

    // Wait for the function to complete or timeout.
    let execution_result = match receiver.recv_timeout(dr_timeout) {
        Ok(_) => handle.join().map_err(|_| VmResultStatus::FailedToJoinThread)?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Handle the timeout case.
            Err(VmResultStatus::ExecutionTimeout)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Handle the case where the thread panicked or the channel was disconnected.
            // This is caused by an error occuring in the thread
            handle.join().map_err(|_| VmResultStatus::FailedToJoinThread)?
        }
    };
    tracing::debug!("VM completed or timed out");

    let mut stdout_buffer = String::new();
    stdout_rx
        .read_to_string(&mut stdout_buffer)
        .map_err(|_| VmResultStatus::FailedToConvertVMPipeToString)?;

    if !stdout_buffer.is_empty() {
        stdout.push(stdout_buffer);
    }

    let mut stderr_buffer = String::new();
    stderr_rx
        .read_to_string(&mut stderr_buffer)
        .map_err(|_| VmResultStatus::FailedToGetWASMStderr)?;

    if !stderr_buffer.is_empty() {
        stderr.push(stderr_buffer);
    }

    execution_result
}

pub fn start_runtime(call_data: VmCallData, context: RuntimeContext) -> VmResult {
    let mut stdout: Vec<String> = vec![];
    let mut stderr: Vec<String> = vec![];

    let vm_execution_result = internal_run_vm(call_data, context, &mut stdout, &mut stderr);

    match vm_execution_result {
        Ok((result, exit_code)) => VmResult {
            stdout,
            stderr,
            exit_info: ExitInfo {
                exit_code,
                exit_message: match exit_code {
                    0 => "Ok".to_string(),
                    _ => String::from_utf8_lossy(&result).to_string(),
                },
            },
            result: Some(result),
        },
        Err(error) => VmResult {
            stdout,
            stderr,
            result: None,
            exit_info: error.into(),
        },
    }
}
