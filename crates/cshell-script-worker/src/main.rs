//! Private, payload-silent offline worker. No daemon, filesystem, network,
//! process, module loader or console API is installed in the JS context.
use cshell_script_runtime::*;
use rquickjs::{Context, Function, Runtime, Value};
mod input;
use input::WorkerInput;
use std::{
    cell::RefCell,
    io::Stdout,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

struct Budget {
    started: Instant,
    blocked_ns: AtomicU64,
    wall: Duration,
    active: Duration,
    exceeded: AtomicBool,
    protocol_failed: AtomicBool,
}
impl Budget {
    fn stop(&self) -> bool {
        let elapsed = self.started.elapsed();
        let active = elapsed.saturating_sub(Duration::from_nanos(
            self.blocked_ns.load(Ordering::Relaxed),
        ));
        if elapsed >= self.wall || active >= self.active {
            self.exceeded.store(true, Ordering::Relaxed);
        }
        self.exceeded.load(Ordering::Relaxed) || self.protocol_failed.load(Ordering::Relaxed)
    }
}
struct Bridge {
    input: WorkerInput,
    output: Stdout,
    start: WorkerStart,
    sequence: u32,
    budget: Arc<Budget>,
}
impl Bridge {
    fn call(&mut self, json: String) -> Result<String, ScriptWorkerError> {
        let json = Zeroizing::new(json);
        if self.budget.stop() {
            return Err(ScriptWorkerError::Protocol);
        }
        if self.sequence > u32::from(self.start.document.limits.max_operations) {
            self.budget.exceeded.store(true, Ordering::Relaxed);
            return Err(ScriptWorkerError::Protocol);
        }
        if json.len() > 16 * 1024 {
            return Err(ScriptWorkerError::Protocol);
        }
        let request: WorkerCall =
            serde_json::from_str(&json).map_err(|_| ScriptWorkerError::Protocol)?;
        let waiting = Instant::now();
        let result = (|| {
            write_frame(
                &mut self.output,
                &WorkerEvent::Call {
                    run_id: self.start.run_id,
                    sequence: self.sequence,
                    request,
                },
            )?;
            let reply = self.input.reply()?;
            if reply.run_id != self.start.run_id || reply.sequence != self.sequence {
                return Err(ScriptWorkerError::Protocol);
            }
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or(ScriptWorkerError::Protocol)?;
            serde_json::to_string(&reply.result).map_err(|_| ScriptWorkerError::Protocol)
        })();
        self.budget.blocked_ns.fetch_add(
            waiting.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        result
    }
}

// Capture the native function and intrinsics before user code can replace them.
// All values and authority are checked again by the parent on every call.
const BOOTSTRAP: &str = r#"
((host) => {
  const parse = JSON.parse, stringify = JSON.stringify, freeze = Object.freeze;
  const FixedError = Error;
  const invoke = (request) => {
    const result = parse(host(stringify(request)));
    if (result.kind === 'denied') throw new FixedError('offline operation denied');
    return result.kind === 'value' || result.kind === 'boolean' ? result.value : undefined;
  };
  Object.defineProperty(globalThis, 'cshell', {value:freeze({
    parameters:freeze({require:async (name) => invoke({kind:'parameter',name})}),
    clock:freeze({sleep:async (milliseconds) => invoke({kind:'sleep',milliseconds})}),
    ui:freeze({notify:async (message) => invoke({kind:'notify',message})}),
    terminal:freeze({
      sendText:async ({text,append_newline=false}) => invoke({kind:'terminal_send',text,append_newline}),
      sendParameter:async ({name,append_newline=false}) => invoke({kind:'terminal_send_parameter',name,append_newline}),
      waitFor:async ({text,timeout_ms}) => invoke({kind:'terminal_wait',text,timeout_ms}),
      waitForParameter:async ({name,timeout_ms}) => invoke({kind:'terminal_wait_parameter',name,timeout_ms})
    }),
    profiles:freeze({read:async () => freeze(parse(invoke({kind:'profile_read'})))}),
    tasks:freeze({submit:async (binding) => invoke({kind:'task_submit',binding})})
  }), writable:false, configurable:false, enumerable:true});
})(globalThis.__cshell_rpc);
delete globalThis.__cshell_rpc;
"#;

fn evaluate(bridge: Rc<RefCell<Bridge>>) -> rquickjs::Result<()> {
    let runtime = Runtime::new()?;
    let (memory, stack, budget) = {
        let bridge = bridge.borrow();
        (
            bridge.start.document.limits.memory_bytes as usize,
            bridge.start.document.limits.stack_bytes as usize,
            bridge.budget.clone(),
        )
    };
    runtime.set_memory_limit(memory);
    runtime.set_max_stack_size(stack);
    let interrupt = budget.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || interrupt.stop())));
    let context = Context::full(&runtime)?;
    context.with(|ctx| {
        let callback_bridge = bridge.clone();
        let callback_budget = budget.clone();
        let callback = Function::new(
            ctx.clone(),
            move |json: String| -> rquickjs::Result<String> {
                callback_bridge.borrow_mut().call(json).map_err(|_| {
                    callback_budget
                        .protocol_failed
                        .store(true, Ordering::Relaxed);
                    rquickjs::Error::new_from_js_message(
                        "host",
                        "reply",
                        "offline host operation failed",
                    )
                })
            },
        )?;
        ctx.globals().set("__cshell_rpc", callback)?;
        ctx.eval::<(), _>(BOOTSTRAP)?;
        let source = Zeroizing::new(bridge.borrow().start.document.source.clone());
        let value: Value<'_> = ctx.eval(source.as_bytes())?;
        if let Some(promise) = value.as_promise() {
            loop {
                if budget.stop() {
                    return Err(rquickjs::Error::WouldBlock);
                }
                if let Some(result) = promise.result::<Value<'_>>() {
                    result?;
                    break;
                }
                if !ctx.execute_pending_job() {
                    return Err(rquickjs::Error::WouldBlock);
                }
            }
        }
        // Returned values and exceptions are never converted to text or sent.
        Ok(())
    })
}
fn run(expected_parent: u32) -> Result<(), ScriptWorkerError> {
    prepare_worker_lifecycle(expected_parent)?;
    let (start, input) = WorkerInput::start()?;
    start.validate()?;
    let containment = enter_worker_containment(&start.document.limits)?;
    let sandbox = enter_worker_access_policy(start.run_id)?;
    let budget = Arc::new(Budget {
        started: Instant::now(),
        blocked_ns: AtomicU64::new(0),
        wall: Duration::from_millis(u64::from(start.document.limits.timeout_ms)),
        // Conservative active wall time excluding host waits, not OS CPU/RSS.
        active: Duration::from_millis(u64::from(start.document.limits.cpu_time_ms)),
        exceeded: AtomicBool::new(false),
        protocol_failed: AtomicBool::new(false),
    });
    let bridge = Rc::new(RefCell::new(Bridge {
        input,
        output: std::io::stdout(),
        start,
        sequence: 1,
        budget: budget.clone(),
    }));
    {
        let mut bridge = bridge.borrow_mut();
        let ready = WorkerEvent::Ready {
            run_id: bridge.start.run_id,
            sequence: 0,
            containment,
            sandbox,
        };
        write_frame(&mut bridge.output, &ready)?;
    }
    let result = evaluate(bridge.clone());
    let stopped = budget.stop();
    let status = if budget.exceeded.load(Ordering::Relaxed) {
        WorkerFinish::BudgetExceeded
    } else if budget.protocol_failed.load(Ordering::Relaxed) {
        WorkerFinish::ProtocolFailed
    } else if result.is_ok() && !stopped {
        WorkerFinish::Completed
    } else {
        WorkerFinish::ScriptFailed
    };
    let mut bridge = bridge.borrow_mut();
    let event = WorkerEvent::Finished {
        run_id: bridge.start.run_id,
        sequence: bridge.sequence,
        status,
    };
    write_frame(&mut bridge.output, &event)
}
fn main() {
    // Never print diagnostics: stdout belongs solely to the private protocol.
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let parent = match args.as_slice() {
        [protocol, option, parent]
            if protocol == WORKER_STDIO_ARGUMENT && option == "--parent-pid" =>
        {
            parent
                .to_str()
                .and_then(|p| p.parse::<u32>().ok())
                .filter(|p| *p > 0)
        }
        _ => None,
    };
    if parent.is_none_or(|parent| run(parent).is_err()) {
        std::process::exit(2);
    }
}
