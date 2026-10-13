//! The dashboard pictures in `docs/images/`. The real view draws each one
//! from a made-up server fed through the same messages real answers arrive
//! as, so a picture cannot show a screen the code no longer draws. Cells
//! become SVG: text where a font draws it well, shapes for the box, block
//! and braille glyphs whose fallback fonts would break the grid.
//!
//! Regenerate with: ULAK_TEST_WRITE_PICTURES=1 cargo test --bins the_pictures_on_disk

use crate::app::tests::{code, driver, key};
use crate::app::{App, Effect, Here};
use crate::engine::{Container, Disk, Image, Info, Network, Snapshot, Stat, Usage, Volume};
use crate::feed::Local;
use crate::feed::Msg;
use crate::intent::{Desired, Status, TunnelState};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const SERVER: &str = "my-server";
const PROJECT: &str = "my-app";

struct Service {
    name: &'static str,
    image: &'static str,
    status: &'static str,
    ports: &'static str,
    /// CPU percent around which the samples wander, and memory in MiB.
    cpu: f64,
    mib: f64,
}

const MY_APP: &[Service] = &[
    Service {
        name: "web",
        image: "nginx:1.27-alpine",
        status: "Up 2 hours (healthy)",
        ports: "0.0.0.0:8080->80/tcp",
        cpu: 1.2,
        mib: 9.4,
    },
    Service {
        name: "api",
        image: "my-app-api",
        status: "Up 2 hours (healthy)",
        ports: "0.0.0.0:3000->3000/tcp",
        cpu: 6.5,
        mib: 182.0,
    },
    Service {
        name: "worker",
        image: "my-app-worker",
        status: "Up 2 hours",
        ports: "",
        cpu: 3.1,
        mib: 96.0,
    },
    Service {
        name: "db",
        image: "postgres:16",
        status: "Up 2 hours (healthy)",
        ports: "5432/tcp",
        cpu: 0.8,
        mib: 64.0,
    },
    Service {
        name: "cache",
        image: "redis:7.4-alpine",
        status: "Up 2 hours",
        ports: "6379/tcp",
        cpu: 0.3,
        mib: 4.1,
    },
];

const BLOG: &[Service] = &[
    Service {
        name: "ghost",
        image: "ghost:5",
        status: "Up 3 days",
        ports: "0.0.0.0:2368->2368/tcp",
        cpu: 0.4,
        mib: 141.0,
    },
    Service {
        name: "mysql",
        image: "mysql:8.4",
        status: "Up 3 days",
        ports: "3306/tcp, 33060/tcp",
        cpu: 0.6,
        mib: 388.0,
    },
];

fn container(project: &str, s: &Service, n: usize) -> Container {
    let name = if project.is_empty() {
        s.name.to_string()
    } else {
        format!("{project}-{}-1", s.name)
    };
    Container {
        id: format!("{:x}{:0>11x}", n + 10, n * 7919 + 104729) + "f00dfeedbeefcafe",
        name,
        image: s.image.into(),
        state: "running".into(),
        status: s.status.into(),
        ports: s.ports.into(),
        project: project.into(),
        service: if project.is_empty() {
            String::new()
        } else {
            s.name.into()
        },
        number: "1".into(),
        networks: if project.is_empty() {
            "bridge".into()
        } else {
            format!("{project}_default")
        },
        ..Container::default()
    }
}

/// A triangle wave between 0 and 1, from integers and exactly rounded
/// arithmetic only: `sin` may differ by an ulp between platforms' maths
/// libraries, and the pictures must come out byte-identical on every runner.
fn tri(t: u64, period: u64) -> f64 {
    let phase = (t % period) as f64 / period as f64;
    (phase * 2.0 - 1.0).abs()
}

/// A wander around `base` with second-to-second noise and the odd burst,
/// different for each container.
fn wander(base: f64, i: u64, seed: u64) -> f64 {
    let t = i + seed * 17;
    let wave = 0.5 * tri(t, 23) + 0.3 * tri(t, 9) + 0.2 * tri(t, 61) - 0.5;
    let noise = ((i * 2_654_435_761 + seed * 40_503) % 1000) as f64 / 1000.0 - 0.5;
    let burst = if (i + seed * 7) % 37 < 3 { 1.6 } else { 0.0 };
    (base * (1.0 + 0.9 * wave + 0.5 * noise + burst)).max(0.05)
}

fn server(now: Instant) -> App {
    let here = Here {
        identity: PROJECT.into(),
        dest: SERVER.into(),
        local_dir: "~/projects/my-app".into(),
        compose: "-f compose.yaml".into(),
        remote_dir: "~/.ulak/workspaces/a1/my-app".into(),
        driver: driver(),
    };
    let mut app = App::new(SERVER.into(), Some(here), Vec::new(), now);
    app.clock = 1_760_000_000;
    let mut containers = Vec::new();
    for (n, s) in MY_APP.iter().enumerate() {
        containers.push(container(PROJECT, s, n));
    }
    for (n, s) in BLOG.iter().enumerate() {
        containers.push(container("blog", s, 10 + n));
    }
    containers.push(container(
        "",
        &Service {
            name: "registry",
            image: "registry:2",
            status: "Up 12 days",
            ports: "0.0.0.0:5000->5000/tcp",
            cpu: 0.1,
            mib: 12.0,
        },
        20,
    ));
    let image = |repository: &str, tag: &str, size: &str, since: &str| Image {
        id: format!("sha256:{:0<64}", repository.len()),
        repository: repository.into(),
        tag: tag.into(),
        created_since: since.into(),
        size: size.into(),
        ..Image::default()
    };
    let images = vec![
        image("my-app-api", "latest", "412MB", "2 hours ago"),
        image("my-app-worker", "latest", "398MB", "2 hours ago"),
        image("nginx", "1.27-alpine", "47.9MB", "3 weeks ago"),
        image("postgres", "16", "438MB", "5 weeks ago"),
        image("redis", "7.4-alpine", "41.4MB", "4 weeks ago"),
        image("ghost", "5", "576MB", "2 months ago"),
        image("mysql", "8.4", "603MB", "2 months ago"),
        image("registry", "2", "25.4MB", "8 months ago"),
    ];
    let volume = |name: &str, project: &str| Volume {
        name: name.into(),
        driver: "local".into(),
        scope: "local".into(),
        project: project.into(),
        ..Volume::default()
    };
    let volumes = vec![
        volume("my-app_pgdata", PROJECT),
        volume("my-app_cache", PROJECT),
        volume("blog_content", "blog"),
        volume("blog_mysql", "blog"),
    ];
    let network = |name: &str, driver: &str, project: &str| Network {
        id: format!("{:0<64}", name.len()),
        name: name.into(),
        driver: driver.into(),
        scope: "local".into(),
        project: project.into(),
        ..Network::default()
    };
    let networks = vec![
        network("bridge", "bridge", ""),
        network("host", "host", ""),
        network("none", "null", ""),
        network("my-app_default", "bridge", PROJECT),
        network("blog_default", "bridge", "blog"),
    ];
    let usage = |kind: &str, total: &str, active: &str, size: &str, reclaimable: &str| Usage {
        kind: kind.into(),
        total: total.into(),
        active: active.into(),
        size: size.into(),
        reclaimable: reclaimable.into(),
    };
    let snap = Snapshot {
        engine: true,
        containers: containers.clone(),
        images: Some(images),
        volumes: Some(volumes),
        networks: Some(networks),
        info: Some(Info {
            version: "29.6.2".into(),
            os: "Ubuntu 24.04.3 LTS".into(),
            arch: "x86_64".into(),
            name: "my-server".into(),
            ncpu: 4,
            mem: 8_162_000_000,
            running: 8,
            containers: 8,
        }),
        disk: Some(Disk {
            total_kb: 80_000_000,
            used_kb: 20_000_000,
            free_kb: 60_000_000,
        }),
        usage: Some(vec![
            usage("Images", "8", "8", "2.54GB", "0B (0%)"),
            usage("Containers", "8", "8", "1.2MB", "0B (0%)"),
            usage("Local Volumes", "4", "4", "1.91GB", "0B (0%)"),
            usage("Build Cache", "41", "0", "1.07GB", "1.07GB"),
        ]),
        trouble: Vec::new(),
    };
    app.on_msg(
        Msg::Snapshot {
            era: 1,
            result: Ok(snap),
            took: Duration::from_millis(180),
        },
        now,
    );
    let services: Vec<&Service> = MY_APP.iter().chain(BLOG).collect();
    for i in 0..crate::app::HISTORY as u64 {
        let at = now + Duration::from_secs(i);
        let samples = containers
            .iter()
            .zip(services.iter().map(|s| (s.cpu, s.mib)).chain([(0.1, 12.0)]))
            .enumerate()
            .map(|(seed, (c, (cpu, mib)))| {
                let cpu = wander(cpu, i, seed as u64);
                let used = mib * (1.0 + 0.08 * (tri(i + seed as u64 * 11, 47) - 0.5));
                Stat {
                    id: c.id[..12].to_string(),
                    name: c.name.clone(),
                    cpu,
                    mem: used / 7784.0 * 100.0,
                    mem_bytes: used * 1_048_576.0,
                    mem_usage: format!("{used:.1}MiB / 7.6GiB"),
                    net_io: format!("{:.1}MB / {:.1}MB", 18.0 + seed as f64, 42.0 + seed as f64),
                    block_io: format!("{:.1}MB / {:.1}MB", 3.0 + seed as f64, 0.5),
                    pids: format!("{}", 4 + seed * 3),
                }
            })
            .collect();
        app.on_msg(Msg::Stats { era: 1, samples }, at);
    }
    app.on_msg(
        Msg::FeedUp {
            era: 1,
            feed: crate::feed::Feed::Events,
        },
        now,
    );
    declare(&mut app, now);
    app
}

/// What the background service says about this project: kept live, with
/// two ports forwarded home.
fn declare(app: &mut App, now: Instant) {
    let desired = Desired {
        schema: crate::intent::SCHEMA,
        live: true,
        workspace_id: "a1".into(),
        workspace_namespace: "n".into(),
        destination: SERVER.into(),
        identity: PROJECT.into(),
        cwd: "/Users/me/projects/my-app".into(),
        argv_globals: Vec::new(),
        compose_env: BTreeMap::new(),
        updated_unix: 1,
    };
    let id = crate::intent::stack_id(SERVER, PROJECT);
    let tunnel = |port, service: &str| TunnelState {
        port,
        service: service.into(),
        open: true,
        service_running: Some(true),
    };
    let status = Status {
        schema: crate::intent::SCHEMA,
        connection: "up".into(),
        tunnels: vec![tunnel(8080, "web"), tunnel(3000, "api")],
        ..Status::default()
    };
    let mut drivers = BTreeMap::new();
    drivers.insert(id.clone(), driver());
    app.on_msg(
        Msg::Local(Box::new(Local {
            catalog: vec![(id.clone(), desired)],
            statuses: vec![(id, status)],
            drivers,
            service_line: "running (launchd), pid 4242".into(),
            service_problem: None,
        })),
        now,
    );
}

/// Moves the cursor down until the named container is under it.
fn select(app: &mut App, name: &str, now: Instant) {
    for _ in 0..40 {
        if let Some(crate::app::Row::Container(i)) = app.selected()
            && app.containers[i].name == name
        {
            return;
        }
        app.on_key(code(KeyCode::Down), now);
    }
    panic!("{name} is not on the list");
}

fn opened(fx: &[Effect]) -> (u64, Vec<String>) {
    fx.iter()
        .find_map(|e| match e {
            Effect::OpenLogs { stream, ids } => Some((*stream, ids.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no log stream opened: {fx:?}"))
}

const LOGS: &[(&str, &str)] = &[
    ("db", "LOG:  database system is ready to accept connections"),
    ("cache", "* Ready to accept connections tcp"),
    ("api", "listening on :3000 (production)"),
    ("worker", "connected to redis://cache:6379, 4 queues"),
    (
        "web",
        "172.18.0.1 - - \"GET / HTTP/1.1\" 200 1843 \"-\" \"Mozilla/5.0\"",
    ),
    ("api", "GET /api/orders?page=1 200 18ms"),
    (
        "web",
        "172.18.0.1 - - \"GET /api/orders?page=1 HTTP/1.1\" 200 5412",
    ),
    ("worker", "job send-receipt#8812 done in 412ms"),
    ("api", "POST /api/checkout 201 96ms"),
    ("worker", "job charge-card#8813 started"),
    ("api", "WARN payment provider slow: 2380ms"),
    (
        "worker",
        "ERROR charge-card#8813: payment provider timeout after 5000ms, retry 1/3",
    ),
    ("db", "LOG:  checkpoint starting: time"),
    ("api", "GET /api/orders/8813 200 11ms"),
    (
        "web",
        "172.18.0.1 - - \"GET /api/orders/8813 HTTP/1.1\" 200 733",
    ),
    ("worker", "job charge-card#8813 started (retry 1/3)"),
    ("worker", "job charge-card#8813 done in 1840ms"),
    ("db", "LOG:  checkpoint complete: wrote 42 buffers (0.3%)"),
    ("api", "GET /healthz 200 1ms"),
    (
        "worker",
        "ERROR sync-inventory#8815: supplier API timeout after 5000ms, retry 1/3",
    ),
    (
        "web",
        "172.18.0.1 - - \"GET /assets/app.3f9c1.js HTTP/1.1\" 200 88201",
    ),
    ("api", "GET /api/products?q=lamp 200 23ms"),
    ("worker", "job sync-inventory#8815 started (retry 1/3)"),
    (
        "cache",
        "1:M 13 Oct 2026 09:41:07.112 * 100 changes in 300 seconds. Saving...",
    ),
    ("worker", "job sync-inventory#8815 done in 2210ms"),
    ("api", "GET /healthz 200 1ms"),
    ("web", "172.18.0.1 - - \"GET /api/cart HTTP/1.1\" 200 412"),
    ("api", "GET /api/cart 200 9ms"),
    ("worker", "job send-receipt#8816 done in 388ms"),
    ("api", "POST /api/checkout 201 102ms"),
    ("worker", "job charge-card#8817 done in 1210ms"),
    (
        "db",
        "LOG:  automatic vacuum of table \"shop.public.orders\"",
    ),
    ("web", "172.18.0.1 - - \"GET /healthz HTTP/1.1\" 200 2"),
    ("api", "GET /api/orders?page=2 200 21ms"),
];

fn feed_logs(app: &mut App, stream: u64, ids: &[String], containers: &[Container]) {
    let source = |service: &str| {
        ids.iter()
            .position(|id| {
                containers
                    .iter()
                    .any(|c| c.service == service && c.id.starts_with(id.as_str()))
            })
            .unwrap_or_else(|| panic!("{service} is not in the stream"))
    };
    for (i, (service, text)) in LOGS.iter().enumerate() {
        let stamp = format!("2026-10-13T09:41:{:02}.{:09}Z", i * 2, 120_000_000 + i * 7);
        app.on_msg(
            Msg::Lines {
                stream,
                source: source(service),
                lines: vec![format!("{stamp} {text}")],
            },
            Instant::now(),
        );
    }
}

struct Picture {
    file: &'static str,
    /// What it shows, for readers who cannot see it.
    alt: &'static str,
    /// Words the screen must hold, so a fixture that stops reaching its
    /// screen fails here instead of drawing the wrong one.
    expect: &'static [&'static str],
    width: u16,
    height: u16,
    app: App,
}

fn pictures() -> Vec<Picture> {
    let now = Instant::now() + Duration::from_secs(1000);
    let at = now + Duration::from_secs(crate::app::HISTORY as u64);
    let mut list = Vec::new();

    let mut a = server(now);
    select(&mut a, "my-app-web-1", at);
    list.push(Picture {
        file: "dashboard-containers.svg",
        alt: "ulak ui: containers grouped by Compose project with live CPU and memory",
        expect: &["kept live by Ulak", "localhost:8080", "my-app-web-1"],
        width: 100,
        height: 24,
        app: a,
    });

    let mut a = server(now);
    let fx = a.on_key(key('l'), at);
    let (stream, ids) = opened(&fx);
    let containers = a.containers.clone();
    feed_logs(&mut a, stream, &ids, &containers);
    // A jump centres its match on the page the view last drew.
    draw(&a, 100, 28);
    a.on_key(key('/'), at);
    for c in "timeout".chars() {
        a.on_key(key(c), at);
    }
    a.on_key(code(KeyCode::Enter), at);
    list.push(Picture {
        file: "dashboard-logs.svg",
        alt: "ulak ui: a stack's logs merged by time, searched for timeout",
        expect: &["/timeout", "2 of 2", "worker │ ERROR"],
        width: 100,
        height: 28,
        app: a,
    });

    let mut a = server(now);
    select(&mut a, "my-app-api-1", at);
    a.on_key(code(KeyCode::Enter), at);
    a.on_key(key('3'), at);
    list.push(Picture {
        file: "dashboard-stats.svg",
        alt: "ulak ui: two minutes of a container's CPU and memory",
        expect: &["3 Stats", "peak", "Memory"],
        width: 100,
        height: 28,
        app: a,
    });

    let mut a = server(now);
    a.on_key(key('D'), at);
    list.push(Picture {
        file: "dashboard-confirm.svg",
        alt: "ulak ui: compose down asks first, with No selected",
        expect: &["Are you sure?", "Take my-app down?"],
        width: 100,
        height: 18,
        app: a,
    });
    list
}

fn draw(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| {
            crate::view::draw(f, app);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

// ─── cells to SVG ───────────────────────────────────────────────────

const CELL_W: f64 = 8.4;
const CELL_H: f64 = 18.0;
const FONT: f64 = 14.0;
const PAD: f64 = 16.0;
const BAR: f64 = 34.0;
const BG: &str = "#1e2128";
const FG: &str = "#d7dae0";
/// The title bar reads like the terminal it was taken in.
const CAPTION: &str = "ulak ui — ~/projects/my-app";

fn rgb(color: Color, fallback: &str) -> String {
    let named = |n: u8| -> &'static str {
        [
            "#1e2128", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
            "#5c6370", "#f0868f", "#acd68e", "#f0cd8c", "#7fc1f5", "#d495e6", "#6ccbd6", "#ffffff",
        ][n as usize]
    };
    match color {
        Color::Reset => fallback.to_string(),
        Color::Black => named(0).into(),
        Color::Red => named(1).into(),
        Color::Green => named(2).into(),
        Color::Yellow => named(3).into(),
        Color::Blue => named(4).into(),
        Color::Magenta => named(5).into(),
        Color::Cyan => named(6).into(),
        Color::Gray => named(7).into(),
        Color::DarkGray => named(8).into(),
        Color::LightRed => named(9).into(),
        Color::LightGreen => named(10).into(),
        Color::LightYellow => named(11).into(),
        Color::LightBlue => named(12).into(),
        Color::LightMagenta => named(13).into(),
        Color::LightCyan => named(14).into(),
        Color::White => named(15).into(),
        Color::Indexed(n) if n < 16 => named(n).into(),
        Color::Indexed(n) if n >= 232 => {
            let v = 8 + 10 * (n - 232);
            format!("#{v:02x}{v:02x}{v:02x}")
        }
        Color::Indexed(n) => {
            let n = n - 16;
            let level = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            format!(
                "#{:02x}{:02x}{:02x}",
                level(n / 36),
                level(n / 6 % 6),
                level(n % 6)
            )
        }
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// Which way a box-drawing glyph's strokes leave its cell: left, right,
/// up, down, and whether they are heavy.
fn strokes(symbol: &str) -> Option<[bool; 5]> {
    Some(match symbol {
        "─" => [true, true, false, false, false],
        "│" => [false, false, true, true, false],
        "━" => [true, true, false, false, true],
        "┃" => [false, false, true, true, true],
        "┌" => [false, true, false, true, false],
        "┐" => [true, false, false, true, false],
        "└" => [false, true, true, false, false],
        "┘" => [true, false, true, false, false],
        "├" => [false, true, true, true, false],
        "┤" => [true, false, true, true, false],
        "┬" => [true, true, false, true, false],
        "┴" => [true, true, true, false, false],
        "┼" => [true, true, true, true, false],
        _ => return None,
    })
}

fn shape(out: &mut String, symbol: &str, x: f64, y: f64, fill: &str) -> bool {
    let (w, h) = (CELL_W, CELL_H);
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    if let Some([l, r, u, d, heavy]) = strokes(symbol) {
        let t = if heavy { 2.4 } else { 1.2 };
        let mut path = String::new();
        if l || r {
            let x0 = if l { x } else { cx - t / 2.0 };
            let x1 = if r { x + w } else { cx + t / 2.0 };
            let _ = write!(path, "M{x0:.2} {:.2}H{x1:.2}v{t}H{x0:.2}Z", cy - t / 2.0);
        }
        if u || d {
            let y0 = if u { y } else { cy - t / 2.0 };
            let y1 = if d { y + h } else { cy + t / 2.0 };
            let _ = write!(path, "M{:.2} {y0:.2}V{y1:.2}h{t}V{y0:.2}Z", cx - t / 2.0);
        }
        let _ = write!(out, r#"<path d="{path}" fill="{fill}"/>"#);
        return true;
    }
    // Rounded corners: a stroke from each edge it touches, bent through
    // the middle of the cell.
    let corner = match symbol {
        "╭" => Some(((x + w, cy), (cx, y + h))),
        "╮" => Some(((x, cy), (cx, y + h))),
        "╰" => Some(((x + w, cy), (cx, y))),
        "╯" => Some(((x, cy), (cx, y))),
        _ => None,
    };
    if let Some(((ax, ay), (bx, by))) = corner {
        let _ = write!(
            out,
            r#"<path d="M{ax:.2} {ay:.2}Q{cx:.2} {cy:.2} {bx:.2} {by:.2}" fill="none" stroke="{fill}" stroke-width="1.2"/>"#
        );
        return true;
    }
    let Some(ch) = symbol
        .chars()
        .next()
        .filter(|_| symbol.chars().count() == 1)
    else {
        return false;
    };
    let o = ch as u32;
    if (0x2581..=0x2588).contains(&o) {
        let part = f64::from(o - 0x2580) / 8.0;
        let _ = write!(
            out,
            r#"<rect x="{x:.2}" y="{:.2}" width="{w:.2}" height="{:.2}" fill="{fill}"/>"#,
            y + h * (1.0 - part),
            h * part
        );
        return true;
    }
    if (0x2800..=0x28ff).contains(&o) {
        let bits = o - 0x2800;
        let dots = [
            (0, 0, 0x01),
            (0, 1, 0x02),
            (0, 2, 0x04),
            (1, 0, 0x08),
            (1, 1, 0x10),
            (1, 2, 0x20),
            (0, 3, 0x40),
            (1, 3, 0x80),
        ];
        for (col, row, bit) in dots {
            if bits & bit != 0 {
                let _ = write!(
                    out,
                    r#"<circle cx="{:.2}" cy="{:.2}" r="1.25" fill="{fill}"/>"#,
                    x + w * (0.3 + 0.4 * f64::from(col)),
                    y + h * (0.14 + 0.24 * f64::from(row))
                );
            }
        }
        return true;
    }
    false
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The colours a cell is drawn in, reversal applied; `None` for a
/// background the window already paints.
fn colours(cell: &ratatui::buffer::Cell) -> (String, Option<String>) {
    let reversed = cell.modifier.contains(Modifier::REVERSED);
    let (fg, bg) = if reversed {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };
    let fill = match fg {
        Color::Reset if reversed => BG.to_string(),
        _ => rgb(fg, FG),
    };
    let back = (bg != Color::Reset || reversed).then(|| rgb(bg, FG));
    let fill = if cell.modifier.contains(Modifier::DIM) {
        format!("{fill}99")
    } else {
        fill
    };
    (fill, back)
}

fn text_style(cell: &ratatui::buffer::Cell, fill: &str) -> String {
    let mut style = format!("fill:{fill}");
    for (modifier, css) in [
        (Modifier::BOLD, ";font-weight:700"),
        (Modifier::ITALIC, ";font-style:italic"),
        (Modifier::UNDERLINED, ";text-decoration:underline"),
    ] {
        if cell.modifier.contains(modifier) {
            style.push_str(css);
        }
    }
    style
}

/// One run of same-styled glyphs on a row, each placed on its own cell so
/// a font a little wider or narrower than the grid cannot drift.
struct Run {
    class: usize,
    glyphs: Vec<(f64, String)>,
}

impl Run {
    fn write(self, out: &mut String, baseline: f64) {
        let xs: Vec<String> = self.glyphs.iter().map(|(x, _)| format!("{x:.1}")).collect();
        let chars: String = self.glyphs.iter().map(|(_, g)| g.as_str()).collect();
        let _ = write!(
            out,
            r#"<text class="c{}" x="{}" y="{baseline:.1}">{}</text>"#,
            self.class,
            xs.join(" "),
            escape(&chars)
        );
    }
}

fn svg(buffer: &Buffer, alt: &str) -> String {
    let (cols, rows) = (buffer.area.width, buffer.area.height);
    let width = f64::from(cols) * CELL_W + 2.0 * PAD;
    let height = BAR + f64::from(rows) * CELL_H + 1.5 * PAD;
    let (ox, oy) = (PAD, BAR + PAD * 0.5);
    let (mut backs, mut shapes, mut text) = (String::new(), String::new(), String::new());
    let mut classes: Vec<String> = Vec::new();
    for y in 0..rows {
        let top = oy + f64::from(y) * CELL_H;
        let baseline = top + CELL_H / 2.0 + FONT * 0.36;
        let mut x = 0;
        while x < cols {
            let Some(back) = colours(&buffer[(x, y)]).1 else {
                x += 1;
                continue;
            };
            let start = x;
            while x < cols && colours(&buffer[(x, y)]).1.as_ref() == Some(&back) {
                x += 1;
            }
            let _ = write!(
                backs,
                r#"<rect x="{:.1}" y="{top:.1}" width="{:.1}" height="{CELL_H}" fill="{back}"/>"#,
                ox + f64::from(start) * CELL_W,
                f64::from(x - start) * CELL_W + 0.4
            );
        }
        let mut run: Option<Run> = None;
        let mut skip = 0;
        for x in 0..cols {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buffer[(x, y)];
            let glyph = cell.symbol();
            skip = glyph.width().saturating_sub(1);
            let left = ox + f64::from(x) * CELL_W;
            let (fill, _) = colours(cell);
            if glyph.trim().is_empty() || shape(&mut shapes, glyph, left, top, &fill) {
                if let Some(done) = run.take() {
                    done.write(&mut text, baseline);
                }
                continue;
            }
            let style = text_style(cell, &fill);
            let class = classes.iter().position(|c| *c == style).unwrap_or_else(|| {
                classes.push(style);
                classes.len() - 1
            });
            match &mut run {
                Some(r) if r.class == class => r.glyphs.push((left, glyph.to_string())),
                _ => {
                    if let Some(done) = run.replace(Run {
                        class,
                        glyphs: vec![(left, glyph.to_string())],
                    }) {
                        done.write(&mut text, baseline);
                    }
                }
            }
        }
        if let Some(done) = run.take() {
            done.write(&mut text, baseline);
        }
    }

    let mut out = String::new();
    let alt = escape(alt);
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.1} {height:.1}" role="img" aria-label="{alt}"><title>{alt}</title>"#
    );
    out.push_str(
        "<style>text{font-family:ui-monospace,'SF Mono',SFMono-Regular,Menlo,Consolas,\
         'Liberation Mono','DejaVu Sans Mono',monospace;font-size:14px;white-space:pre}",
    );
    for (i, style) in classes.iter().enumerate() {
        let _ = write!(out, ".c{i}{{{style}}}");
    }
    out.push_str("</style>");
    // The window: a rounded frame, a title bar and its three buttons.
    let (right, inner) = (width - 0.5, width - 10.5);
    let _ = write!(
        out,
        r##"<rect x="0.5" y="0.5" width="{:.1}" height="{:.1}" rx="10" fill="{BG}" stroke="#3b404a"/>"##,
        width - 1.0,
        height - 1.0
    );
    let _ = write!(
        out,
        r##"<path d="M0.5 {BAR}V10.5A10 10 0 0 1 10.5 0.5H{inner:.1}A10 10 0 0 1 {right:.1} 10.5V{BAR}Z" fill="#2a2e36"/><path d="M0.5 {BAR}H{right:.1}" stroke="#3b404a"/>"##
    );
    for (i, colour) in ["#ff5f57", "#febc2e", "#28c840"].iter().enumerate() {
        let _ = write!(
            out,
            r#"<circle cx="{}" cy="{}" r="6" fill="{colour}"/>"#,
            20 + i * 20,
            BAR / 2.0
        );
    }
    let _ = write!(
        out,
        r##"<text x="{:.1}" y="{:.1}" text-anchor="middle" style="fill:#8b919c;font-size:13px">{CAPTION}</text>"##,
        width / 2.0,
        BAR / 2.0 + 4.5
    );
    out.push_str(&backs);
    out.push_str(&shapes);
    out.push_str(&text);
    out.push_str("</svg>\n");
    out
}

fn screen(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Regenerate with: ULAK_TEST_WRITE_PICTURES=1 cargo test --bins the_pictures_on_disk
#[test]
fn the_pictures_on_disk_are_what_the_view_draws() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/images");
    let write = std::env::var_os("ULAK_TEST_WRITE_PICTURES").is_some();
    let list = pictures();
    assert!(!list.is_empty());
    for p in list {
        let fresh = svg(&draw(&p.app, p.width, p.height), p.alt);
        let path = dir.join(p.file);
        if write {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &fresh).unwrap();
            continue;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == fresh,
            "{} no longer matches the view; regenerate with: \
             ULAK_TEST_WRITE_PICTURES=1 cargo test --bins the_pictures_on_disk",
            path.display()
        );
    }
}

#[test]
fn every_picture_shows_the_screen_its_caption_names() {
    for p in pictures() {
        let text = screen(&draw(&p.app, p.width, p.height));
        for word in p.expect {
            assert!(text.contains(word), "{} lacks {word:?}:\n{text}", p.file);
        }
    }
}

#[test]
fn box_block_and_braille_glyphs_are_drawn_and_text_is_escaped() {
    let mut out = String::new();
    for glyph in ["─", "╭", "▅", "⣿"] {
        assert!(shape(&mut out, glyph, 0.0, 0.0, FG), "{glyph}");
    }
    assert!(!shape(&mut out, "a", 0.0, 0.0, FG));
    assert!(!shape(&mut out, "●", 0.0, 0.0, FG));
    assert_eq!(
        escape(r#"<a href="x">&</a>"#),
        "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;"
    );
}
