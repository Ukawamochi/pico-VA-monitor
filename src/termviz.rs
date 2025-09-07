//! ASCII バー/ミニメータ描画（no_std）
//! - `pct(x, max)` で 0..=100[%] 正規化
//! - `render_bar(percent, buf)` で `====>.....` 風バー文字列を生成

#![allow(dead_code)]

use core::str;

pub const BAR_W: usize = 32;

/// 値 x を [0, max] に正規化して 0..=100[%] を返す（飽和）
pub fn pct(x: f32, max: f32) -> u8 {
    if !(x.is_finite()) || max <= 0.0 { return 0; }
    let p = (x / max) * 100.0;
    if p <= 0.0 { 0 } else if p >= 100.0 { 100 } else { p as u8 }
}

/// 与えた%に応じて `=====>.....` 形式のバーを生成して `&str` を返す
/// バッファは呼び出し側に `[u8; BAR_W]` を用意させる（no_std対応）
pub fn render_bar(percent: u8, buf: &mut [u8; BAR_W]) -> &str {
    let filled = ((percent as usize) * (BAR_W - 1)) / 100; // 最終1文字は余白/末尾
    for i in 0..BAR_W { buf[i] = b'.'; }
    if filled > 0 {
        for i in 0..filled.min(BAR_W - 1) { buf[i] = b'='; }
        if filled < BAR_W { buf[filled] = b'>'; }
    } else {
        buf[0] = b'>';
    }
    // 安全：ASCIIのみを書き込む
    unsafe { str::from_utf8_unchecked(&buf[..]) }
}

/// 整形1行の保持構造体（必要なら使用）。
/// defmt では任意整形を制御しづらいため、本サンプルでは
/// 直接 `render_bar` + 数値で出力する方針を採用。
#[derive(defmt::Format)]
pub struct TermLine<'a> {
    pub label: &'a str,
    pub value: f32,
    pub unit: &'a str,
    pub percent: u8,
    pub bar: &'a str,
}

/// 単位と値を揃えて1行整形の素材を作る（ラベル/値/単位/バー/％）
pub fn line<'a>(label: &'a str, value: f32, unit: &'a str, max: f32, buf: &'a mut [u8; BAR_W]) -> TermLine<'a> {
    let percent = pct(value, max);
    let bar = render_bar(percent, buf);
    TermLine { label, value, unit, percent, bar }
}

