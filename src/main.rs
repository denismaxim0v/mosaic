#[cfg(test)]
mod tests;

mod boundaries;
mod command_is_executing;
mod errors;
mod input;
mod layout;
mod os_input_output;
mod pty_bus;
mod screen;
mod tab;
mod terminal_pane;
mod utils;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{channel, sync_channel, Receiver, SendError, Sender, SyncSender};
use std::thread;

use serde::{Deserialize, Serialize};
use structopt::StructOpt;

use crate::command_is_executing::CommandIsExecuting;
use crate::errors::{AppContext, ContextType, ErrorContext, PtyContext, ScreenContext};
use crate::input::input_loop;
use crate::layout::Layout;
use crate::os_input_output::{get_os_input, OsApi};
use crate::pty_bus::{PtyBus, PtyInstruction, VteEvent};
use crate::screen::{Screen, ScreenInstruction};
use crate::utils::{
    consts::{MOSAIC_IPC_PIPE, MOSAIC_TMP_DIR, MOSAIC_TMP_LOG_DIR},
    logging::*,
};
use std::cell::RefCell;

thread_local!(static OPENCALLS: RefCell<ErrorContext> = RefCell::new(ErrorContext::new()));

#[derive(Serialize, Deserialize, Debug)]
enum ApiCommand {
    OpenFile(PathBuf),
    SplitHorizontally,
    SplitVertically,
    MoveFocus,
}

#[derive(Clone)]
enum SenderType<T: Clone> {
    Sender(Sender<(T, ErrorContext)>),
    SyncSender(SyncSender<(T, ErrorContext)>),
}

#[derive(Clone)]
pub struct SenderWithContext<T: Clone> {
    err_ctx: ErrorContext,
    sender: SenderType<T>,
}

impl<T: Clone> SenderWithContext<T> {
    fn new(err_ctx: ErrorContext, sender: SenderType<T>) -> Self {
        Self { err_ctx, sender }
    }

    pub fn send(&self, event: T) -> Result<(), SendError<(T, ErrorContext)>> {
        match self.sender {
            SenderType::Sender(ref s) => s.send((event, self.err_ctx)),
            SenderType::SyncSender(ref s) => s.send((event, self.err_ctx)),
        }
    }

    pub fn update(&mut self, new_ctx: ErrorContext) {
        self.err_ctx = new_ctx;
    }
}

unsafe impl<T: Clone> Send for SenderWithContext<T> {}
unsafe impl<T: Clone> Sync for SenderWithContext<T> {}

#[derive(StructOpt, Debug, Default)]
#[structopt(name = "mosaic")]
pub struct Opt {
    #[structopt(short, long)]
    /// Send "split (direction h == horizontal / v == vertical)" to active mosaic session
    split: Option<char>,
    #[structopt(short, long)]
    /// Send "move focused pane" to active mosaic session
    move_focus: bool,
    #[structopt(short, long)]
    /// Send "open file in new pane" to active mosaic session
    open_file: Option<PathBuf>,
    #[structopt(long)]
    /// Maximum panes on screen, caution: opening more panes will close old ones
    max_panes: Option<usize>,
    #[structopt(short, long)]
    /// Path to a layout yaml file
    layout: Option<PathBuf>,
    #[structopt(short, long)]
    debug: bool,
}

pub fn main() {
    let opts = Opt::from_args();
    if let Some(split_dir) = opts.split {
        match split_dir {
            'h' => {
                let mut stream = UnixStream::connect(MOSAIC_IPC_PIPE).unwrap();
                let api_command = bincode::serialize(&ApiCommand::SplitHorizontally).unwrap();
                stream.write_all(&api_command).unwrap();
            }
            'v' => {
                let mut stream = UnixStream::connect(MOSAIC_IPC_PIPE).unwrap();
                let api_command = bincode::serialize(&ApiCommand::SplitVertically).unwrap();
                stream.write_all(&api_command).unwrap();
            }
            _ => {}
        };
    } else if opts.move_focus {
        let mut stream = UnixStream::connect(MOSAIC_IPC_PIPE).unwrap();
        let api_command = bincode::serialize(&ApiCommand::MoveFocus).unwrap();
        stream.write_all(&api_command).unwrap();
    } else if let Some(file_to_open) = opts.open_file {
        let mut stream = UnixStream::connect(MOSAIC_IPC_PIPE).unwrap();
        let api_command = bincode::serialize(&ApiCommand::OpenFile(file_to_open)).unwrap();
        stream.write_all(&api_command).unwrap();
    } else {
        let os_input = get_os_input();
        atomic_create_dir(MOSAIC_TMP_DIR).unwrap();
        atomic_create_dir(MOSAIC_TMP_LOG_DIR).unwrap();
        start(Box::new(os_input), opts);
    }
}

#[derive(Clone)]
pub enum AppInstruction {
    Exit,
    Error(String),
}

pub fn start(mut os_input: Box<dyn OsApi>, opts: Opt) {
    let mut active_threads = vec![];

    let command_is_executing = CommandIsExecuting::new();

    let full_screen_ws = os_input.get_terminal_size_using_fd(0);
    os_input.into_raw_mode(0);
    let (send_screen_instructions, receive_screen_instructions): (
        Sender<(ScreenInstruction, ErrorContext)>,
        Receiver<(ScreenInstruction, ErrorContext)>,
    ) = channel();
    let err_ctx = OPENCALLS.with(|ctx| *ctx.borrow());
    let mut send_screen_instructions =
        SenderWithContext::new(err_ctx, SenderType::Sender(send_screen_instructions));
    let (send_pty_instructions, receive_pty_instructions): (
        Sender<(PtyInstruction, ErrorContext)>,
        Receiver<(PtyInstruction, ErrorContext)>,
    ) = channel();
    let mut send_pty_instructions =
        SenderWithContext::new(err_ctx, SenderType::Sender(send_pty_instructions));
    let (send_app_instructions, receive_app_instructions): (
        SyncSender<(AppInstruction, ErrorContext)>,
        Receiver<(AppInstruction, ErrorContext)>,
    ) = sync_channel(0);
    let send_app_instructions =
        SenderWithContext::new(err_ctx, SenderType::SyncSender(send_app_instructions));
    let mut screen = Screen::new(
        receive_screen_instructions,
        send_pty_instructions.clone(),
        send_app_instructions.clone(),
        &full_screen_ws,
        os_input.clone(),
        opts.max_panes,
    );
    let mut pty_bus = PtyBus::new(
        receive_pty_instructions,
        send_screen_instructions.clone(),
        os_input.clone(),
        opts.debug,
    );
    let maybe_layout = opts.layout.map(Layout::new);

    #[cfg(not(test))]
    std::panic::set_hook({
        use crate::errors::handle_panic;
        let send_app_instructions = send_app_instructions.clone();
        Box::new(move |info| {
            handle_panic(info, &send_app_instructions);
        })
    });

    active_threads.push(
        thread::Builder::new()
            .name("pty".to_string())
            .spawn({
                let mut command_is_executing = command_is_executing.clone();
                move || {
                    if let Some(layout) = maybe_layout {
                        pty_bus.spawn_terminals_for_layout(layout);
                    } else {
                        let pid = pty_bus.spawn_terminal(None);
                        pty_bus
                            .send_screen_instructions
                            .send(ScreenInstruction::NewTab(pid))
                            .unwrap();
                    }

                    loop {
                        let (event, mut err_ctx) = pty_bus
                            .receive_pty_instructions
                            .recv()
                            .expect("failed to receive event on channel");
                        err_ctx.add_call(ContextType::Pty(PtyContext::from(&event)));
                        pty_bus.send_screen_instructions.update(err_ctx);
                        match event {
                            PtyInstruction::SpawnTerminal(file_to_open) => {
                                let pid = pty_bus.spawn_terminal(file_to_open);
                                pty_bus
                                    .send_screen_instructions
                                    .send(ScreenInstruction::NewPane(pid))
                                    .unwrap();
                            }
                            PtyInstruction::SpawnTerminalVertically(file_to_open) => {
                                let pid = pty_bus.spawn_terminal(file_to_open);
                                pty_bus
                                    .send_screen_instructions
                                    .send(ScreenInstruction::VerticalSplit(pid))
                                    .unwrap();
                            }
                            PtyInstruction::SpawnTerminalHorizontally(file_to_open) => {
                                let pid = pty_bus.spawn_terminal(file_to_open);
                                pty_bus
                                    .send_screen_instructions
                                    .send(ScreenInstruction::HorizontalSplit(pid))
                                    .unwrap();
                            }
                            PtyInstruction::NewTab => {
                                let pid = pty_bus.spawn_terminal(None);
                                pty_bus
                                    .send_screen_instructions
                                    .send(ScreenInstruction::NewTab(pid))
                                    .unwrap();
                            }
                            PtyInstruction::ClosePane(id) => {
                                pty_bus.close_pane(id);
                                command_is_executing.done_closing_pane();
                            }
                            PtyInstruction::CloseTab(ids) => {
                                pty_bus.close_tab(ids);
                                command_is_executing.done_closing_pane();
                            }
                            PtyInstruction::Quit => {
                                break;
                            }
                        }
                    }
                }
            })
            .unwrap(),
    );

    active_threads.push(
        thread::Builder::new()
            .name("screen".to_string())
            .spawn({
                let mut command_is_executing = command_is_executing.clone();
                move || loop {
                    let (event, mut err_ctx) = screen
                        .receiver
                        .recv()
                        .expect("failed to receive event on channel");
                    err_ctx.add_call(ContextType::Screen(ScreenContext::from(&event)));
                    screen.send_app_instructions.update(err_ctx);
                    screen.send_pty_instructions.update(err_ctx);
                    match event {
                        ScreenInstruction::Pty(pid, vte_event) => {
                            screen
                                .get_active_tab_mut()
                                .unwrap()
                                .handle_pty_event(pid, vte_event);
                        }
                        ScreenInstruction::Render => {
                            screen.render();
                        }
                        ScreenInstruction::NewPane(pid) => {
                            screen.get_active_tab_mut().unwrap().new_pane(pid);
                            command_is_executing.done_opening_new_pane();
                        }
                        ScreenInstruction::HorizontalSplit(pid) => {
                            screen.get_active_tab_mut().unwrap().horizontal_split(pid);
                            command_is_executing.done_opening_new_pane();
                        }
                        ScreenInstruction::VerticalSplit(pid) => {
                            screen.get_active_tab_mut().unwrap().vertical_split(pid);
                            command_is_executing.done_opening_new_pane();
                        }
                        ScreenInstruction::WriteCharacter(bytes) => {
                            screen
                                .get_active_tab_mut()
                                .unwrap()
                                .write_to_active_terminal(bytes);
                        }
                        ScreenInstruction::ResizeLeft => {
                            screen.get_active_tab_mut().unwrap().resize_left();
                        }
                        ScreenInstruction::ResizeRight => {
                            screen.get_active_tab_mut().unwrap().resize_right();
                        }
                        ScreenInstruction::ResizeDown => {
                            screen.get_active_tab_mut().unwrap().resize_down();
                        }
                        ScreenInstruction::ResizeUp => {
                            screen.get_active_tab_mut().unwrap().resize_up();
                        }
                        ScreenInstruction::MoveFocus => {
                            screen.get_active_tab_mut().unwrap().move_focus();
                        }
                        ScreenInstruction::MoveFocusLeft => {
                            screen.get_active_tab_mut().unwrap().move_focus_left();
                        }
                        ScreenInstruction::MoveFocusDown => {
                            screen.get_active_tab_mut().unwrap().move_focus_down();
                        }
                        ScreenInstruction::MoveFocusRight => {
                            screen.get_active_tab_mut().unwrap().move_focus_right();
                        }
                        ScreenInstruction::MoveFocusUp => {
                            screen.get_active_tab_mut().unwrap().move_focus_up();
                        }
                        ScreenInstruction::ScrollUp => {
                            screen
                                .get_active_tab_mut()
                                .unwrap()
                                .scroll_active_terminal_up();
                        }
                        ScreenInstruction::ScrollDown => {
                            screen
                                .get_active_tab_mut()
                                .unwrap()
                                .scroll_active_terminal_down();
                        }
                        ScreenInstruction::ClearScroll => {
                            screen
                                .get_active_tab_mut()
                                .unwrap()
                                .clear_active_terminal_scroll();
                        }
                        ScreenInstruction::CloseFocusedPane => {
                            screen.get_active_tab_mut().unwrap().close_focused_pane();
                            screen.render();
                        }
                        ScreenInstruction::ClosePane(id) => {
                            screen.get_active_tab_mut().unwrap().close_pane(id);
                            screen.render();
                        }
                        ScreenInstruction::ToggleActiveTerminalFullscreen => {
                            screen
                                .get_active_tab_mut()
                                .unwrap()
                                .toggle_active_terminal_fullscreen();
                        }
                        ScreenInstruction::NewTab(pane_id) => {
                            screen.new_tab(pane_id);
                            command_is_executing.done_opening_new_pane();
                        }
                        ScreenInstruction::SwitchTabNext => screen.switch_tab_next(),
                        ScreenInstruction::SwitchTabPrev => screen.switch_tab_prev(),
                        ScreenInstruction::CloseTab => screen.close_tab(),
                        ScreenInstruction::ApplyLayout((layout, new_pane_pids)) => {
                            screen.apply_layout(layout, new_pane_pids)
                        }
                        ScreenInstruction::Quit => {
                            break;
                        }
                    }
                }
            })
            .unwrap(),
    );

    // Here be dragons! This is very much a work in progress, and isn't quite functional
    // yet. It's being left out of the tests because is slows them down massively (by
    // recompiling a WASM module for every single test). Stay tuned for more updates!
    #[cfg(feature = "wasm-wip")]
    active_threads.push(
        thread::Builder::new()
            .name("wasm".to_string())
            .spawn(move || {
                // TODO: Clone shared state here
                move || -> Result<(), Box<dyn std::error::Error>> {
                    use std::io;
                    use std::sync::{Arc, Mutex};
                    use wasmer::{Exports, Function, Instance, Module, Store, Value};
                    use wasmer_wasi::WasiState;
                    let store = Store::default();

                    println!("Compiling module...");
                    // FIXME: Switch to a higher performance compiler (`Store::default()`) and cache this on disk
                    // I could use `(de)serialize_to_file()` for that
                    let module = if let Ok(m) = Module::from_file(&store, "strider.wasm") {
                        m
                    } else {
                        return Ok(()); // Just abort this thread quietly if the WASM isn't found
                    };

                    // FIXME: Upstream the `Pipe` struct
                    //let output = fluff::Pipe::new();
                    //let input = fluff::Pipe::new();
                    let mut wasi_env = WasiState::new("mosaic")
                        .env("CLICOLOR_FORCE", "1")
                        .preopen(|p| {
                            p.directory(".") // TODO: Change this to a more meaningful dir
                                .alias(".")
                                .read(true)
                                .write(true)
                                .create(true)
                        })?
                        //.stdin(Box::new(input))
                        //.stdout(Box::new(output))
                        .finalize()?;

                    let mut import_object = wasi_env.import_object(&module)?;
                    // FIXME: Upstream an `ImportObject` merge method
                    let mut host_exports = Exports::new();
                    /* host_exports.insert(
                        "host_open_file",
                        Function::new_native_with_env(&store, Arc::clone(&wasi_env.state), host_open_file),
                    ); */
                    fn noop() {}
                    host_exports.insert("host_open_file", Function::new_native(&store, noop));
                    import_object.register("mosaic", host_exports);
                    let instance = Instance::new(&module, &import_object)?;

                    // WASI requires to explicitly set the memory for the `WasiEnv`
                    wasi_env.set_memory(instance.exports.get_memory("memory")?.clone());

                    let start = instance.exports.get_function("_start")?;
                    let handle_key = instance.exports.get_function("handle_key")?;
                    let draw = instance.exports.get_function("draw")?;

                    // This eventually calls the `.init()` method
                    start.call(&[])?;

                    #[warn(clippy::never_loop)]
                    loop {
                        break;
                        //let (cols, rows) = terminal::size()?;
                        //draw.call(&[Value::I32(rows as i32), Value::I32(cols as i32)])?;

                        // FIXME: This downcasting mess needs to be abstracted away
                        /* let mut state = wasi_env.state();
                        let wasi_file = state.fs.stdout_mut()?.as_mut().unwrap();
                        let output: &mut fluff::Pipe = wasi_file.downcast_mut().unwrap();
                        // Needed because raw mode doesn't implicitly return to the start of the line
                        write!(
                            io::stdout(),
                            "{}\n\r",
                            output.to_string().lines().collect::<Vec<_>>().join("\n\r")
                        )?;
                        output.clear();

                        let wasi_file = state.fs.stdin_mut()?.as_mut().unwrap();
                        let input: &mut fluff::Pipe = wasi_file.downcast_mut().unwrap();
                        input.clear(); */

                        /* match event::read()? {
                            Event::Key(KeyEvent {
                                code: KeyCode::Char('q'),
                                ..
                            }) => break,
                            Event::Key(e) => {
                                writeln!(input, "{}\r", serde_json::to_string(&e)?)?;
                                drop(state);
                                // Need to release the implicit `state` mutex or I deadlock!
                                handle_key.call(&[])?;
                            }
                            _ => (),
                        } */
                    }
                    debug_log_to_file("WASM module loaded and exited cleanly :)".to_string())?;
                    Ok(())
                }()
                .unwrap()
            })
            .unwrap(),
    );

    // TODO: currently we don't push this into active_threads
    // because otherwise the app will hang. Need to fix this so it both
    // listens to the ipc-bus and is able to quit cleanly
    #[cfg(not(test))]
    let _ipc_thread = thread::Builder::new()
        .name("ipc_server".to_string())
        .spawn({
            use std::io::Read;
            let mut send_pty_instructions = send_pty_instructions.clone();
            let mut send_screen_instructions = send_screen_instructions.clone();
            move || {
                std::fs::remove_file(MOSAIC_IPC_PIPE).ok();
                let listener = std::os::unix::net::UnixListener::bind(MOSAIC_IPC_PIPE)
                    .expect("could not listen on ipc socket");
                let mut err_ctx = OPENCALLS.with(|ctx| *ctx.borrow());
                err_ctx.add_call(ContextType::IPCServer);
                send_pty_instructions.update(err_ctx);
                send_screen_instructions.update(err_ctx);

                for stream in listener.incoming() {
                    match stream {
                        Ok(mut stream) => {
                            let mut buffer = [0; 65535]; // TODO: more accurate
                            let _ = stream
                                .read(&mut buffer)
                                .expect("failed to parse ipc message");
                            let decoded: ApiCommand = bincode::deserialize(&buffer)
                                .expect("failed to deserialize ipc message");
                            match &decoded {
                                ApiCommand::OpenFile(file_name) => {
                                    let path = PathBuf::from(file_name);
                                    send_pty_instructions
                                        .send(PtyInstruction::SpawnTerminal(Some(path)))
                                        .unwrap();
                                }
                                ApiCommand::SplitHorizontally => {
                                    send_pty_instructions
                                        .send(PtyInstruction::SpawnTerminalHorizontally(None))
                                        .unwrap();
                                }
                                ApiCommand::SplitVertically => {
                                    send_pty_instructions
                                        .send(PtyInstruction::SpawnTerminalVertically(None))
                                        .unwrap();
                                }
                                ApiCommand::MoveFocus => {
                                    send_screen_instructions
                                        .send(ScreenInstruction::MoveFocus)
                                        .unwrap();
                                }
                            }
                        }
                        Err(err) => {
                            panic!("err {:?}", err);
                        }
                    }
                }
            }
        })
        .unwrap();

    let _stdin_thread = thread::Builder::new()
        .name("stdin_handler".to_string())
        .spawn({
            let send_screen_instructions = send_screen_instructions.clone();
            let send_pty_instructions = send_pty_instructions.clone();
            let os_input = os_input.clone();
            move || {
                input_loop(
                    os_input,
                    command_is_executing,
                    send_screen_instructions,
                    send_pty_instructions,
                    send_app_instructions,
                )
            }
        });

    #[warn(clippy::never_loop)]
    loop {
        let (app_instruction, mut err_ctx) = receive_app_instructions
            .recv()
            .expect("failed to receive app instruction on channel");

        err_ctx.add_call(ContextType::App(AppContext::from(&app_instruction)));
        send_screen_instructions.update(err_ctx);
        send_pty_instructions.update(err_ctx);
        match app_instruction {
            AppInstruction::Exit => {
                let _ = send_screen_instructions.send(ScreenInstruction::Quit);
                let _ = send_pty_instructions.send(PtyInstruction::Quit);
                break;
            }
            AppInstruction::Error(backtrace) => {
                let _ = send_screen_instructions.send(ScreenInstruction::Quit);
                let _ = send_pty_instructions.send(PtyInstruction::Quit);
                os_input.unset_raw_mode(0);
                let goto_start_of_last_line = format!("\u{1b}[{};{}H", full_screen_ws.rows, 1);
                let error = format!("{}\n{}", goto_start_of_last_line, backtrace);
                let _ = os_input
                    .get_stdout_writer()
                    .write(error.as_bytes())
                    .unwrap();
                for thread_handler in active_threads {
                    let _ = thread_handler.join();
                }
                std::process::exit(1);
            }
        }
    }

    for thread_handler in active_threads {
        thread_handler.join().unwrap();
    }
    // cleanup();
    let reset_style = "\u{1b}[m";
    let show_cursor = "\u{1b}[?25h";
    let goto_start_of_last_line = format!("\u{1b}[{};{}H", full_screen_ws.rows, 1);
    let goodbye_message = format!(
        "{}\n{}{}Bye from Mosaic!",
        goto_start_of_last_line, reset_style, show_cursor
    );

    os_input.unset_raw_mode(0);
    let _ = os_input
        .get_stdout_writer()
        .write(goodbye_message.as_bytes())
        .unwrap();
    os_input.get_stdout_writer().flush().unwrap();
}
