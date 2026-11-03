//! Non-Windows builds: a scripted simulator that feeds synthetic "Windows"
//! input through the real engine and network stack. Used for end-to-end
//! testing against a client running with --dry-run.

use crate::control::Control;
use crate::engine::{Action, Engine, Input};
use crate::layout::{Layout, Rect};
use crate::Opts;
use glidekm_core::info;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn feed_with(e: &mut Engine, cursor: &mut (i32, i32), inp: Input) {
    let block = e.input(inp);
    if let Input::Move { new, .. } = inp {
        if !block {
            *cursor = new; // Windows applies unblocked moves
        }
    }
    for a in e.actions.drain(..) {
        if let Action::Warp(x, y) = a {
            *cursor = (x, y);
        }
        info!("sim: action {a:?}");
    }
    std::thread::sleep(Duration::from_millis(5));
}

/// Parse "l,t,r,b;l,t,r,b" (for --monitors).
fn parse_monitors(s: &str) -> Option<Vec<Rect>> {
    s.split(';')
        .map(|m| {
            let v: Vec<i32> = m
                .split(',')
                .map(|x| x.trim().parse().ok())
                .collect::<Option<_>>()?;
            (v.len() == 4).then(|| Rect {
                l: v[0],
                t: v[1],
                r: v[2],
                b: v[3],
            })
        })
        .collect()
}

pub fn run(ctl: Arc<Control>, opts: Opts) -> Result<(), String> {
    let simulate = opts.flags.flag("simulate");
    let monitors = parse_monitors(&opts.flags.str_or("monitors", "0,0,1920,1080"))
        .ok_or("invalid --monitors")?;
    *ctl.monitors.lock().unwrap() = monitors.clone();
    if !simulate {
        return Err(
            "glidekm-server captures input only on Windows. On other systems use --simulate for testing."
                .into(),
        );
    }
    let tx = crate::start_services(&ctl)?;
    let shared = ctl.shared.clone();
    let p = ctl.cfg.lock().unwrap().engine_params()?;
    if !shared.configured() {
        return Err("--simulate needs --client and --passphrase".into());
    }
    let layout = Layout {
        monitors,
        side: p.side,
        offset: p.offset,
        scale: p.scale,
        client_w: 0,
        client_h: 0,
    };
    let mut e = Engine::new(layout, tx, shared.clone(), p.hk_switch, p.hk_capture);
    let mut cursor = (1800i32, 500i32);

    let t0 = Instant::now();
    while !shared.alive.load(Ordering::Relaxed) {
        if t0.elapsed() > Duration::from_secs(10) {
            return Err("client did not come online".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    info!("sim: client online, size {:?}", shared.size());

    let mv = |c: (i32, i32), dx: i32, dy: i32| Input::Move {
        old: c,
        new: (c.0 + dx, c.1 + dy),
        injected: false,
    };

    // Move right until we cross into the client.
    for _ in 0..20 {
        let i = mv(cursor, 10, 0);
        feed_with(&mut e, &mut cursor, i);
    }
    info!("sim: remote={} cursor={cursor:?}", e.is_remote());
    // Type "hi" (scan codes 0x23, 0x17), click, scroll.
    for (vk, sc) in [(0x48u32, 0x23u32), (0x49, 0x17)] {
        feed_with(
            &mut e,
            &mut cursor,
            Input::Key {
                vk,
                scan: sc,
                extended: false,
                down: true,
            },
        );
        feed_with(
            &mut e,
            &mut cursor,
            Input::Key {
                vk,
                scan: sc,
                extended: false,
                down: false,
            },
        );
    }
    feed_with(&mut e, &mut cursor, Input::Button { idx: 0, down: true });
    feed_with(
        &mut e,
        &mut cursor,
        Input::Button {
            idx: 0,
            down: false,
        },
    );
    feed_with(
        &mut e,
        &mut cursor,
        Input::Wheel {
            horizontal: false,
            delta: 120,
        },
    );
    // Hold a key across the switch back: the client must release it on Leave.
    feed_with(
        &mut e,
        &mut cursor,
        Input::Key {
            vk: 0x41,
            scan: 0x1E,
            extended: false,
            down: true,
        },
    );
    std::thread::sleep(Duration::from_millis(400));
    // Toggle capture mode (Ctrl+Alt+Shift+M), relative motion, toggle back.
    let chord = [(0xA2u32, 0x1Du32), (0xA4, 0x38), (0xA0, 0x2A)];
    for &(vk, sc) in &chord {
        feed_with(
            &mut e,
            &mut cursor,
            Input::Key {
                vk,
                scan: sc,
                extended: false,
                down: true,
            },
        );
    }
    feed_with(
        &mut e,
        &mut cursor,
        Input::Key {
            vk: 0x4D,
            scan: 0x32,
            extended: false,
            down: true,
        },
    );
    feed_with(
        &mut e,
        &mut cursor,
        Input::Key {
            vk: 0x4D,
            scan: 0x32,
            extended: false,
            down: false,
        },
    );
    feed_with(&mut e, &mut cursor, Input::Raw { dx: 15, dy: -4 });
    feed_with(
        &mut e,
        &mut cursor,
        Input::Key {
            vk: 0x4D,
            scan: 0x32,
            extended: false,
            down: true,
        },
    );
    feed_with(
        &mut e,
        &mut cursor,
        Input::Key {
            vk: 0x4D,
            scan: 0x32,
            extended: false,
            down: false,
        },
    );
    for &(vk, sc) in &chord {
        feed_with(
            &mut e,
            &mut cursor,
            Input::Key {
                vk,
                scan: sc,
                extended: false,
                down: false,
            },
        );
    }
    // Move back left onto Windows.
    let park = cursor;
    for _ in 0..30 {
        if !e.is_remote() {
            break;
        }
        feed_with(&mut e, &mut cursor, mv(park, -10, 0));
    }
    info!("sim: remote={} cursor={cursor:?}", e.is_remote());
    std::thread::sleep(Duration::from_millis(500));
    feed_with(&mut e, &mut cursor, Input::Tick);
    info!("sim: done");
    Ok(())
}
