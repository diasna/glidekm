//! Non-Windows builds: a scripted simulator that feeds synthetic "Windows"
//! input through the real engine and network stack. Used for end-to-end
//! testing against a client running with --dry-run.

use crate::engine::{Action, Engine, Input};
use crate::layout::{Layout, Rect};
use crate::{net, Settings};
use glidekm_core::info;
use std::sync::atomic::Ordering;
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

pub fn run(s: Settings) -> Result<(), String> {
    if !s.cfg.flag("simulate") {
        return Err("glidekm-server only captures input on Windows. On other systems use --simulate for testing.".into());
    }
    let shared = net::Shared::new();
    let tx = net::spawn(s.client.clone(), s.psk, shared.clone()).map_err(|e| e.to_string())?;
    let layout = Layout {
        monitors: vec![Rect {
            l: 0,
            t: 0,
            r: 1920,
            b: 1080,
        }],
        side: s.side,
        offset: s.offset,
        scale: s.scale,
        client_w: 0,
        client_h: 0,
    };
    let mut e = Engine::new(layout, tx, shared.clone(), s.hk_switch, s.hk_capture);
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
