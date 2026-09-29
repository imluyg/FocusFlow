//! M1 的浏览器侧核对：吃 `.scratch/run_annot_mode_harness.sh` 的产物（真 `snip.js` 跑出来的
//! 标注层与它自报的换算结果），核四件事：
//! 1. 页面那份 `cssRectToPhysical` 镜像与 Rust 的 `css_rect_to_physical` **逐矩形相等**
//!    （含"不足 1 像素 → 两边都要出声"那一支）；
//! 2. 真页面交回来的图层 PNG 尺寸严格等于 Rust 为同一个选区算出的物理尺寸 ——
//!    不等就是 `snip_commit` 会被拒，用户在标注完的那一刻才看到红字；
//! 3. 那一层能 1:1 合成到底图上（不缩放、不补边）；
//! 4. 红线在真图层上再钉一次：未被笔碰到的像素逐字节相等。
//!
//! 用法（由 runner 自己调）：`cargo run -p focusflow-core --example annot_m1_check -- <meta.txt> <layer.png>`

use std::process::exit;

use focusflow_core::capture::{self, CssRect, MonitorRect};

struct Verdict {
    fails: usize,
}

impl Verdict {
    fn check(&mut self, name: &str, ok: bool, extra: String) {
        println!("RESULT::M1_{} {name}", if ok { "PASS" } else { "FAIL" });
        println!("  {extra}");
        if !ok {
            self.fails += 1;
        }
    }
}

fn f(s: &str) -> f64 {
    s.trim().parse().unwrap_or(f64::NAN)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(meta_path), Some(layer_path)) = (args.first(), args.get(1)) else {
        eprintln!("用法: annot_m1_check <meta.txt> <layer.png>");
        exit(2);
    };
    let text = std::fs::read_to_string(meta_path).expect("读不到 meta");
    let layer_bytes = std::fs::read(layer_path).expect("读不到标注层 PNG");
    let mut v = Verdict { fails: 0 };

    let mut grid: Option<(f64, u32, u32, String)> = None;
    let mut sel: Option<(f64, CssRect, u32, u32)> = None;
    for line in text.lines() {
        let mut it = line.trim().splitn(2, ' ');
        let (tag, rest) = match (it.next(), it.next()) {
            (Some(t), Some(r)) => (t, r),
            _ => continue,
        };
        match tag {
            "GRID" => {
                // dpr|W,H|case;case;...   每个 case 是 x,y,w,h,px,py,pw,ph（后四个可能是 null）
                let p: Vec<&str> = rest.split('|').collect();
                if p.len() == 3 {
                    let mon: Vec<&str> = p[1].split(',').collect();
                    grid = Some((
                        f(p[0]),
                        f(mon[0]) as u32,
                        f(mon[1]) as u32,
                        p[2].to_string(),
                    ));
                }
            }
            "SEL" => {
                // dpr|x,y,w,h|画布宽,画布高
                let p: Vec<&str> = rest.split('|').collect();
                if p.len() == 3 {
                    let b: Vec<f64> = p[1].split(',').map(f).collect();
                    let c: Vec<f64> = p[2].split(',').map(f).collect();
                    if b.len() == 4 && c.len() == 2 {
                        sel = Some((
                            f(p[0]),
                            CssRect {
                                x: b[0],
                                y: b[1],
                                w: b[2],
                                h: b[3],
                            },
                            c[0] as u32,
                            c[1] as u32,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // —— 1) 换算镜像逐条对拍
    let Some((dpr, mw, mh, cases)) = grid else {
        v.check("G0 夹具报了 GRID 行", false, "没有 GRID 行".to_string());
        println!("RESULT::M1_DONE fails={}", v.fails);
        exit(1);
    };
    let mon = MonitorRect::new(0, 0, mw, mh).unwrap();
    let mut n_cases = 0usize;
    let mut diffs: Vec<String> = Vec::new();
    for row in cases.split(';').filter(|r| !r.trim().is_empty()) {
        let p: Vec<&str> = row.split(',').collect();
        if p.len() != 8 {
            diffs.push(format!("这一行字段不齐：{row}"));
            continue;
        }
        n_cases += 1;
        let r = CssRect {
            x: f(p[0]),
            y: f(p[1]),
            w: f(p[2]),
            h: f(p[3]),
        };
        let got = capture::css_rect_to_physical(&r, dpr, &mon);
        if p[4] == "null" {
            if let Ok(ok) = got {
                diffs.push(format!(
                    "{:?} 页面判成不可用，Rust 却算出了 {}x{}@{},{}",
                    r, ok.width, ok.height, ok.x, ok.y
                ));
            }
            continue;
        }
        match got {
            Ok(ok) => {
                let rust = [ok.x as f64, ok.y as f64, ok.width as f64, ok.height as f64];
                let page = [f(p[4]), f(p[5]), f(p[6]), f(p[7])];
                if rust != page {
                    diffs.push(format!("{r:?} Rust {rust:?} 页面 {page:?}"));
                }
            }
            Err(e) => diffs.push(format!("{r:?} 页面给了 {:?} 而 Rust 报错：{e}", &p[4..8])),
        }
    }
    v.check(
        "G1 页面的换算镜像与 Rust 逐矩形相等",
        n_cases > 0 && diffs.is_empty(),
        format!("{n_cases} 个矩形，{diffs:?}"),
    );

    // —— 2) 真页面那一次的提交体：图层尺寸必须严格等于 Rust 为同一选区算出的尺寸
    let Some((sdpr, srect, cw, ch)) = sel else {
        v.check("S0 夹具报了 SEL 行", false, "没有 SEL 行".to_string());
        println!("RESULT::M1_DONE fails={}", v.fails);
        exit(1);
    };
    let sphys = capture::css_rect_to_physical(&srect, sdpr, &mon).expect("选区算不出来");
    v.check(
        "S1 页面画布位图尺寸 == Rust 为同一选区算出的物理尺寸",
        (sphys.width, sphys.height) == (cw, ch),
        format!("Rust {}x{} 页面 {cw}x{ch}", sphys.width, sphys.height),
    );

    let (layer, lw, lh) = match capture::decode_png_rgba(&layer_bytes) {
        Ok(t) => t,
        Err(e) => {
            v.check("S2 页面交回来的图层解得开", false, e);
            println!("RESULT::M1_DONE fails={}", v.fails);
            exit(1);
        }
    };
    v.check(
        "S2 图层是 8 位 RGBA 且尺寸严格等于选区（Rust 不缩放）",
        (lw, lh) == (sphys.width, sphys.height),
        format!(
            "解码 {lw}x{lh} 选区 {}x{}，PNG {} 字节",
            sphys.width,
            sphys.height,
            layer_bytes.len()
        ),
    );
    if (lw, lh) != (sphys.width, sphys.height) {
        println!("RESULT::M1_DONE fails={}", v.fails);
        exit(1);
    }

    let painted = layer.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count();
    v.check(
        "S3 那一笔真的在图层里（不是全透明的一张空层）",
        painted > 0,
        format!("alpha>0 的格 {painted} / 共 {}", lw as usize * lh as usize),
    );

    // —— 3+4) 合成到底图上：尺寸闸门通过，且未覆盖的像素逐字节相等
    let mut dst: Vec<u8> = Vec::new();
    for i in 0..(lw as usize * lh as usize) {
        let n = (i as u32 * 7 + 13) % 199;
        dst.extend_from_slice(&[n as u8, (n + 31) as u8, (n + 61) as u8, 0]);
    }
    let before = dst.clone();
    let ok = capture::composite_over(&mut dst, &layer, lw, lh);
    v.check(
        "C1 真图层能合成上去（composite_over 的尺寸闸门通过）",
        ok.is_ok(),
        format!("{ok:?}"),
    );
    let mut stray = 0usize;
    let mut touched = 0usize;
    for (idx, (g, o)) in dst
        .as_chunks::<4>()
        .0
        .iter()
        .zip(before.as_chunks::<4>().0.iter())
        .enumerate()
    {
        let a = layer.as_chunks::<4>().0[idx][3];
        if a == 0 {
            if g != o {
                stray += 1;
            }
        } else {
            touched += 1;
            // 覆盖到的那一格 alpha 仍是 0（底图那第四字节谁都不许碰）
            if g[3] != 0 {
                stray += 1;
            }
        }
    }
    v.check(
        "C2 笔没碰到的像素逐字节相等，且底图 alpha 一个没改",
        stray == 0,
        format!("越界改动 {stray} 格 / 覆盖 {touched} 格"),
    );

    println!("RESULT::M1_DONE fails={}", v.fails);
    exit(if v.fails == 0 { 0 } else { 1 });
}
