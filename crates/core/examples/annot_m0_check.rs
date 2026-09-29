//! M0 的浏览器侧核对（`docs/snip-annotate-plan.md` §三 M0 第 4 条）。
//!
//! 吃 `.scratch/run_annot_harness.sh` 产出的两件东西：真浏览器 canvas 导出的标注层 PNG，
//! 以及页面自报的换算数值（CSS 选区、它自己算出的物理格、它自己回读到的像素包围盒）。
//! 然后用**产品那条链路**核四件事：
//! 1. `css_rect_to_physical` 算出的选区物理矩形 == 页面算的（两套换算必须同一条规则）；
//! 2. 解码出来的层尺寸严格等于选区尺寸（不等就报错，绝不缩放）；
//! 3. 合成 → 编码 → 再解码之后，标注块的像素包围盒与页面所见误差 ≤1px，且纯红仍是纯红；
//! 4. 标注块之外的像素逐字节等于未标注时那张（红线）。
//!
//! 用法（由 `run_annot_harness.sh` 自己调）：
//! `cargo run -p focusflow-core --example annot_m0_check -- <meta.txt> <layer.png>`

use std::process::exit;

use focusflow_core::capture::{self, CssRect, MonitorRect, Shot};

struct Mark {
    name: String,
    css: [f64; 4],
    canvas: [i64; 4],
    bbox: [i64; 5],
    rgba: [u8; 4],
}

#[derive(Default)]
struct Meta {
    dpr: f64,
    mon: [u32; 2],
    sel: [f64; 4],
    sel_phys: [i64; 4],
}

struct Verdict {
    fails: usize,
}

impl Verdict {
    fn check(&mut self, name: &str, ok: bool, extra: String) {
        let tag = if ok { "PASS" } else { "FAIL" };
        println!("RESULT::M0_{tag} {name}");
        println!("  {extra}");
        if !ok {
            self.fails += 1;
        }
    }
}

fn nums(s: &str) -> Vec<f64> {
    s.split(',').filter_map(|v| v.trim().parse().ok()).collect()
}

fn ints(s: &str) -> Vec<i64> {
    s.split(',').filter_map(|v| v.trim().parse().ok()).collect()
}

fn parse_meta(text: &str) -> (Meta, Vec<Mark>) {
    let mut meta = Meta::default();
    let mut marks = Vec::new();
    for line in text.lines() {
        let mut it = line.trim().splitn(2, ' ');
        let (tag, rest) = match (it.next(), it.next()) {
            (Some(t), Some(r)) => (t, r),
            _ => continue,
        };
        let mut kv = std::collections::BTreeMap::new();
        let parts: Vec<&str> = rest.split('|').collect();
        for pair in parts.chunks(2) {
            if pair.len() == 2 {
                kv.insert(pair[0].to_string(), pair[1].to_string());
            }
        }
        let get = |k: &str| -> String { kv.get(k).cloned().unwrap_or_default() };
        match tag {
            "META" => {
                meta.dpr = get("dpr").parse().unwrap_or(1.0);
                let m = nums(&get("mon"));
                meta.mon = [m[0] as u32, m[1] as u32];
                meta.sel = nums(&get("sel")).try_into().unwrap();
                meta.sel_phys = ints(&get("selPhys")).try_into().unwrap();
            }
            "MARK" => {
                marks.push(Mark {
                    name: get("name"),
                    css: nums(&get("css")).try_into().unwrap(),
                    canvas: ints(&get("canvas")).try_into().unwrap(),
                    bbox: ints(&get("bbox")).try_into().unwrap(),
                    rgba: nums(&get("rgba"))
                        .into_iter()
                        .map(|v| v as u8)
                        .collect::<Vec<u8>>()
                        .try_into()
                        .unwrap(),
                });
            }
            _ => {}
        }
    }
    (meta, marks)
}

/// 与用例里同一个 LCG：值域压到 0..=199、alpha 一律 0（GDI 交回的就是 0）。
fn noise_bgra(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w as usize) * (h as usize) * 4);
    let mut s = 0x2545_F491_u32;
    for _ in 0..(w as usize * h as usize) {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = (s >> 16) as u8;
        v.extend_from_slice(&[n % 200, n / 2 % 200, n.wrapping_add(37) % 200, 0]);
    }
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(meta_path), Some(layer_path)) = (args.first(), args.get(1)) else {
        eprintln!("用法: annot_m0_check <meta.txt> <layer.png>");
        exit(2);
    };
    let text = std::fs::read_to_string(meta_path).expect("读不到 meta");
    let png_bytes = std::fs::read(layer_path).expect("读不到标注层 PNG");
    let (meta, marks) = parse_meta(&text);
    let mut v = Verdict { fails: 0 };

    v.check(
        "M0 夹具真的报出了标注层",
        !marks.is_empty() && png_bytes.len() > 8 && &png_bytes[1..4] == b"PNG",
        format!("笔 {} 支，PNG {} 字节", marks.len(), png_bytes.len()),
    );

    // 1) 选区的物理矩形：Rust 的真函数 vs 页面自己算的
    let mon = MonitorRect::new(0, 0, meta.mon[0], meta.mon[1]).unwrap();
    let sel = CssRect {
        x: meta.sel[0],
        y: meta.sel[1],
        w: meta.sel[2],
        h: meta.sel[3],
    };
    let sp = capture::css_rect_to_physical(&sel, meta.dpr, &mon).unwrap();
    let rust_rect = [sp.x as i64, sp.y as i64, sp.width as i64, sp.height as i64];
    v.check(
        "M1 选区换算两边一致（页面与 Rust 同一条 round 规则）",
        rust_rect == meta.sel_phys,
        format!("Rust {rust_rect:?} 页面 {:?}", meta.sel_phys),
    );

    // 2) 解码：尺寸必须严格等于选区，不等就是页面画错或有人偷偷重采样
    let (layer, lw, lh) = match capture::decode_png_rgba(&png_bytes) {
        Ok(t) => t,
        Err(e) => {
            v.check("M2 页面导出的 PNG 能解码", false, e);
            println!("RESULT::M0_DONE fails=1");
            exit(1);
        }
    };
    v.check(
        "M2 解码尺寸严格等于选区物理尺寸（绝不缩放）",
        (lw, lh) == (sp.width, sp.height),
        format!("解码 {lw}x{lh} 选区 {}x{}", sp.width, sp.height),
    );
    if (lw, lh) != (sp.width, sp.height) {
        println!("RESULT::M0_DONE fails=1");
        exit(1);
    }

    // 3) 合成到裁剪后的 BGRA 上（等价于 snip.rs:612 之后、617 之前那一步）
    let shot = Shot::new(mon, noise_bgra(mon.width, mon.height)).unwrap();
    let cropped = shot
        .crop(sp.x as u32, sp.y as u32, sp.width, sp.height)
        .unwrap();
    let mut dst = cropped.bgra.clone();
    if let Err(e) = capture::composite_over(&mut dst, &layer, sp.width, sp.height) {
        v.check("M3 合成", false, e);
        println!("RESULT::M0_DONE fails=1");
        exit(1);
    }
    // 落盘那张：与 snip.rs 完全同一步（clone → bgra_to_rgba → encode_png）
    let mut encoded = dst.clone();
    capture::bgra_to_rgba(&mut encoded);
    let out_png = capture::encode_png(&encoded, sp.width, sp.height).unwrap();
    let (final_px, fw, fh) = capture::decode_png_rgba(&out_png).unwrap();
    v.check(
        "M3 最终 PNG 就是选区那么大",
        (fw, fh) == (sp.width, sp.height),
        format!("{fw}x{fh}，{} 字节", out_png.len()),
    );

    // 未标注时的那张，用来逐字节比「标注块之外」
    let mut untouched = cropped.bgra.clone();
    capture::bgra_to_rgba(&mut untouched);
    let cw = sp.width as usize;
    let ch = sp.height as usize;
    let px = |buf: &[u8], x: usize, y: usize| -> [u8; 4] {
        let i = (y * cw + x) * 4;
        buf[i..i + 4].try_into().unwrap()
    };

    let mut marker_pixels = vec![false; cw * ch];
    let mut expect_changed = 0usize;
    for m in &marks {
        // 页面报的笔是「相对选区原点的 CSS」，换成绝对 CSS 再走 Rust 的真函数
        let pen = CssRect {
            x: meta.sel[0] + m.css[0],
            y: meta.sel[1] + m.css[1],
            w: m.css[2],
            h: m.css[3],
        };
        let pp = capture::css_rect_to_physical(&pen, meta.dpr, &mon).unwrap();
        let exp = [
            (pp.x - sp.x) as i64,
            (pp.y - sp.y) as i64,
            pp.width as i64,
            pp.height as i64,
        ];
        v.check(
            &format!("M4 {}：笔格换算两边一致", m.name),
            exp == m.canvas,
            format!("Rust {exp:?} 页面 {:?}", m.canvas),
        );
        expect_changed += (exp[2] * exp[3]) as usize;

        // 在最终 PNG 里量「被改动的像素」的包围盒（半透明那支认不了颜色，按差异认）
        let pad = 4i64;
        let (x0, y0) = ((exp[0] - pad).max(0), (exp[1] - pad).max(0));
        let (x1, y1) = (
            (exp[0] + exp[2] + pad).min(cw as i64 - 1),
            (exp[1] + exp[3] + pad).min(ch as i64 - 1),
        );
        let (mut min_x, mut min_y, mut max_x, mut max_y, mut n) =
            (usize::MAX, usize::MAX, 0usize, 0usize, 0usize);
        for y in y0 as usize..=y1 as usize {
            for x in x0 as usize..=x1 as usize {
                let a = px(&final_px, x, y);
                let b = px(&untouched, x, y);
                if a[..3] == b[..3] {
                    continue;
                }
                marker_pixels[y * cw + x] = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                n += 1;
            }
        }
        let got = [min_x as i64, min_y as i64, max_x as i64, max_y as i64];
        let want_page = [m.bbox[0], m.bbox[1], m.bbox[2], m.bbox[3]];
        v.check(
            &format!("M5 {}：最终 PNG 的标注块与页面所见误差 ≤1px", m.name),
            n > 0
                && got
                    .iter()
                    .zip(want_page.iter())
                    .all(|(g, w)| (g - w).abs() <= 1),
            format!("最终 {got:?} 页面 {want_page:?}"),
        );
        v.check(
            &format!("M5b {}：标注块像素数与页面回读一致", m.name),
            n as i64 == m.bbox[4] && n == (exp[2] * exp[3]) as usize,
            format!("最终 {n} 页面 {} 期望 {}", m.bbox[4], exp[2] * exp[3]),
        );
        let off = [
            (min_x as i64 - exp[0]).abs(),
            (min_y as i64 - exp[1]).abs(),
            (max_x as i64 - (exp[0] + exp[2] - 1)).abs(),
            (max_y as i64 - (exp[1] + exp[3] - 1)).abs(),
        ];
        v.check(
            &format!("M6 {}：相对 Rust 换算式偏不到 1px", m.name),
            off.iter().all(|d| *d <= 1),
            format!("左{} 上{} 右{} 下{}", off[0], off[1], off[2], off[3]),
        );

        // 不透明那支还要断颜色：通道序写反了这里必红（页面画的纯红要落成纯红）
        if m.rgba == [255, 0, 0, 255] {
            let all_red = (min_y..=max_y)
                .all(|y| (min_x..=max_x).all(|x| px(&final_px, x, y) == [255, 0, 0, 255]));
            v.check(
                "M7 纯红绕一圈回来还是纯红（RGBA↔BGRA 没写反）",
                all_red,
                format!("块内 {n} 格"),
            );
        }
    }

    // 4) 红线另一半：所有标注块之外的像素逐字节相等
    let mut stray = 0usize;
    for y in 0..ch {
        for x in 0..cw {
            let i = (y * cw + x) * 4;
            if marker_pixels[y * cw + x] {
                continue;
            }
            if final_px[i..i + 4] != untouched[i..i + 4] {
                stray += 1;
            }
        }
    }
    v.check(
        "M8 未被标注覆盖的像素逐字节相等",
        stray == 0,
        format!("越界改动 {stray} 格，共 {} 格", cw * ch),
    );
    let changed = marker_pixels.iter().filter(|b| **b).count();
    v.check(
        "M9 改动像素总数等于各标注块面积之和（没有渗到别处）",
        changed == expect_changed,
        format!("改动 {changed} 格，期望 {expect_changed} 格"),
    );

    println!("RESULT::M0_DONE fails={}", v.fails);
    exit(if v.fails == 0 { 0 } else { 1 });
}
