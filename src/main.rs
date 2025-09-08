#![no_std]
#![no_main]

// 本ファイルは「最小限の機能」に絞った版です。
// 目的:
//  - INA219 の電圧/電流/電力を 500ms 間隔で defmt に出力
//  - 起動からの経過時間（s）と、累計消費電力量（mWh）を併せて表示
// 設計方針:
//  - エネルギーは整数で積算（単位: µW・ms）。表示時に mWh へ換算。
//  - 積算は「前回の有効電力値」を区間一定として dt（ms）で台形ではなく矩形近似。
//    next_measurement() が毎回新値を返す前提なら誤差は小さい。

use cortex_m_rt::entry;
use defmt::*;
use defmt_rtt as _;
use fugit::RateExtU32 as _;
use panic_probe as _;

use embedded_hal::delay::DelayNs;
use hal::{
    clocks::init_clocks_and_plls, gpio::FunctionI2C, pac, sio::Sio, watchdog::Watchdog, Timer, I2C,
};
use rp2040_hal as hal;
use rp2040_hal::Clock;

// INA219（同期API）
use ina219 as ina;
use ina219::address::Address;
use ina219::calibration::IntCalibration;
use ina219::configuration::{BusVoltageRange, Configuration, ShuntVoltageRange};
use ina219::errors::InitializationErrorReason;

// ---- 設定定数（必要最小限） ----
const SHUNT_OHMS: f32 = 0.1; // シャント抵抗 [Ω]
const MAX_EXPECTED_AMPS: f32 = 2.0; // 最大期待電流 [A]
const LOOP_MS: u32 = 500; // 計測周期 [ms]
const INA_ADDR: u8 = 0x44; //INA219の半田によってアドレスが変わります

#[entry]
fn main() -> ! {
    // PAC 取得
    let mut pac = pac::Peripherals::take().unwrap();
    let _core = pac::CorePeripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);

    // クロック初期化（外部XOSC 12MHz 前提）
    let clocks = init_clocks_and_plls(
        12_000_000u32,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    // SIO / GPIO
    let sio = Sio::new(pac.SIO);
    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // I2C0 @ 100kHz（安定性重視）
    // 外部プルアップ（4.7kΩ〜10kΩ）を前提。pull-up を有効化してから I2C 機能へ切り替える。
    let sda = pins
        .gpio4
        .into_pull_up_input()
        .into_function::<FunctionI2C>();
    let scl = pins
        .gpio5
        .into_pull_up_input()
        .into_function::<FunctionI2C>();
    let i2c = I2C::i2c0(
        pac.I2C0,
        sda,
        scl,
        // 100kHz 程度が無難。配線に問題がなければ 400kHz まで引き上げ可能。
        100.kHz(),
        &mut pac.RESETS,
        // system_clock 周波数を渡す（I2C タイミング計算に使用）。
        // rp2040-hal の例と同様に system_clock を指定するのが正。
        clocks.system_clock.freq(),
    );

    // タイマ（Δt計測 & ウェイト）
    let mut timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);
    // RTT アタッチ猶予（ホストが接続する時間を与える）
    timer.delay_ms(500u32);
    info!("=== PICO INA219 MINIMAL ===");
    info!("Boot OK. Init INA219...");

    // INA219 初期化（使用アドレスは `INA_ADDR`）
    let mut ina = match init_ina219(i2c) {
        Ok(device) => {
            info!("INA219 init: OK");
            device
        }
        Err(_) => {
            error!("INA219 init: NG - 配線/電源/アドレスを確認してください");
            error!("Expected connections:");
            error!("  VCC -> Pico 3V3");
            error!("  GND -> Pico GND");
            error!("  SDA -> Pico GPIO4");
            error!("  SCL -> Pico GPIO5");
            error!("  VIN+/VIN- -> measurement circuit");
            error!("INA219 address: 0x{=u8:x} (default)", INA_ADDR);
            core::panic!("INA219 init failed")
        }
    };

    // ループ（最小出力）
    info!("Start loop: print V/I/P every {=u32} ms", LOOP_MS);
    // 積算用の基準時刻/前回時刻
    let start = timer.get_counter();
    let mut last = start;
    // 累積エネルギー（µW・ms）: last_p_uw * dt_ms を逐次加算
    let mut energy_uWms: i64 = 0;
    // 積算に用いる直近の電力（µW）。新しいサンプルが来る度に更新。
    let mut last_p_uw: i64 = 0;
    // 分ごとの集計（時間重み付き平均と消費エネルギー）
    let mut last_ms_total: u64 = 0; // start を 0ms とする絶対経過msの前回値
    let mut min_v_mV_ms: i64 = 0; // V[mV]×ms の積算
    let mut min_i_uA_ms: i64 = 0; // I[µA]×ms の積算
    let mut min_energy_uWms: i64 = 0; // P[µW]×ms の積算（その分だけ）
    let mut min_duration_ms: u64 = 0; // その分に積算したms（理想は60,000）
    let mut minute_count: u64 = 0; // 何分目（1始まり）
    // 時間重み用に保持する直近の V/I（サンプル到来時に更新）
    let mut last_v_mv: i32 = 0;
    let mut last_i_ua: i32 = 0;
    // 1秒ごと表示のための直近出力秒
    let mut last_printed_sec: u64 = 0;
    loop {
        let now = timer.get_counter();
        // 経過時間と微小区間 dt（ms）を取得
        let dt_ms_u64: u64 = (now - last).to_millis() as u64;
        last = now; // 次回用に更新
        // エネルギー積算（µW・ms）。矩形近似で last_p_uw を使用。
        energy_uWms = energy_uWms.saturating_add(last_p_uw.saturating_mul(dt_ms_u64 as i64));
        // 総経過時間（ms）
        let elapsed_ms_total: u64 = (now - start).to_millis() as u64;
        let curr_sec: u64 = elapsed_ms_total / 1000;

        // 1分区切りの時間重み付き積算（分境界をまたぐ場合は分割）
        let mut remain = elapsed_ms_total.saturating_sub(last_ms_total);
        while remain > 0 {
            let this_min = last_ms_total / 60_000;
            let next_boundary = (this_min + 1) * 60_000;
            let step = core::cmp::min(remain, next_boundary - last_ms_total);
            let step_i64 = step as i64;

            // 分の積算（矩形近似）
            min_energy_uWms = min_energy_uWms.saturating_add(last_p_uw.saturating_mul(step_i64));
            min_v_mV_ms = min_v_mV_ms
                .saturating_add((last_v_mv as i64).saturating_mul(step_i64));
            min_i_uA_ms = min_i_uA_ms
                .saturating_add((last_i_ua as i64).saturating_mul(step_i64));
            min_duration_ms = min_duration_ms.saturating_add(step);

            last_ms_total = last_ms_total.saturating_add(step);
            remain -= step;

            // 分境界に到達したら出力してリセット
            if last_ms_total % 60_000 == 0 {
                minute_count = minute_count.saturating_add(1);

                // 平均 V/I を算出（整数で切り捨て）
                let avg_v_mv: i32 = if min_duration_ms > 0 {
                    (min_v_mV_ms / min_duration_ms as i64) as i32
                } else {
                    0
                };
                let avg_i_ua: i32 = if min_duration_ms > 0 {
                    (min_i_uA_ms / min_duration_ms as i64) as i32
                } else {
                    0
                };

                // V: 2桁整数＋3桁小数（固定幅）
                let v_mv_u32: u32 = if avg_v_mv > 0 { avg_v_mv as u32 } else { 0 };
                let v_int_raw = v_mv_u32 / 1000;
                let v_int = core::cmp::min(v_int_raw, 99);
                let v_frac = v_mv_u32 % 1000;
                let v_i_t = ((v_int / 10) % 10) as u8;
                let v_i_o = (v_int % 10) as u8;
                let v_f_h = ((v_frac / 100) % 10) as u8;
                let v_f_t = ((v_frac / 10) % 10) as u8;
                let v_f_o = (v_frac % 10) as u8;

                // I: 4桁整数＋1桁小数（固定幅）
                let i_ua_u32: u32 = if avg_i_ua > 0 { avg_i_ua as u32 } else { 0 };
                let i_mAx10 = i_ua_u32 / 100; // mA×10
                let i_int_raw = i_mAx10 / 10;
                let i_int = core::cmp::min(i_int_raw, 9_999);
                let i_frac1 = i_mAx10 % 10;
                let i_i_th = ((i_int / 1000) % 10) as u8;
                let i_i_h = ((i_int / 100) % 10) as u8;
                let i_i_t = ((i_int / 10) % 10) as u8;
                let i_i_o = (i_int % 10) as u8;
                let i_f_o = i_frac1 as u8;

                // その1分で消費した電池 %（小数2桁、切り捨て、最大 999.99）
                let e_pos: u128 = if min_energy_uWms > 0 { min_energy_uWms as u128 } else { 0 };
                let pct_x100: u128 = e_pos
                    .saturating_mul(10_000) // 100×100
                    / 9_000_000_000_000u128; // 2.5Wh を µW・ms に換算
                let pct_int_raw: u64 = (pct_x100 / 100) as u64;
                let pct_int = core::cmp::min(pct_int_raw, 999);
                let pct_frac2: u8 = (pct_x100 % 100) as u8;
                let pct_d3 = ((pct_int / 100) % 10) as u8;
                let pct_d2 = ((pct_int / 10) % 10) as u8;
                let pct_d1 = (pct_int % 10) as u8;
                let pct_f1: char = (b'0' + (pct_frac2 / 10)) as char;
                let pct_f2: char = (b'0' + (pct_frac2 % 10)) as char;

                // 何分目（2桁）
                let min_cap = core::cmp::min(minute_count, 99);
                let mn_t = ((min_cap / 10) % 10) as u8;
                let mn_o = (min_cap % 10) as u8;

                info!(
                    "{=char}{=char}分目  平均: V={=char}{=char}.{=char}{=char}{=char} V  I={=char}{=char}{=char}{=char}.{=char} mA  |  1分消費: AA={=char}{=char}{=char}.{=char}{=char}%",
                    (b'0' + mn_t) as char, (b'0' + mn_o) as char,
                    // V
                    (b'0' + v_i_t) as char, (b'0' + v_i_o) as char,
                    (b'0' + v_f_h) as char, (b'0' + v_f_t) as char,
                    (b'0' + v_f_o) as char,
                    // I
                    (b'0' + i_i_th) as char, (b'0' + i_i_h) as char,
                    (b'0' + i_i_t) as char, (b'0' + i_i_o) as char,
                    (b'0' + i_f_o) as char,
                    // %
                    (b'0' + pct_d3) as char, (b'0' + pct_d2) as char,
                    (b'0' + pct_d1) as char, pct_f1, pct_f2
                );

                // リセット（次の1分へ）
                min_v_mV_ms = 0;
                min_i_uA_ms = 0;
                min_energy_uWms = 0;
                min_duration_ms = 0;
            }
        }

        match ina_next(&mut ina) {
            Ok(Some((v_mv, i_ua, p_uw))) => {
                // 単位変換は整数ベースで行い、出力は固定幅・ゼロ埋めで桁をそろえる
                // 電圧: v_mv [mV] -> 00.000 V（2桁整数＋3桁小数）
                let v_mv_u32: u32 = if v_mv > 0 { v_mv as u32 } else { 0 };
                let v_int: u32 = v_mv_u32 / 1000; // 0..32
                let v_frac: u32 = v_mv_u32 % 1000; // 0..999
                let v_i_t = ((v_int / 10) % 10) as u8;
                let v_i_o = (v_int % 10) as u8;
                let v_f_h = ((v_frac / 100) % 10) as u8;
                let v_f_t = ((v_frac / 10) % 10) as u8;
                let v_f_o = (v_frac % 10) as u8;

                // 電流: i_ua [µA] -> 0000.0 mA（4桁整数＋1桁小数）
                let i_ua_u32: u32 = if i_ua > 0 { i_ua as u32 } else { 0 };
                let i_max10: u32 = i_ua_u32 / 100; // mA×10
                let i_int: u32 = i_max10 / 10; // 0..9999
                let i_frac1: u32 = i_max10 % 10; // 0..9
                let i_i_th = ((i_int / 1000) % 10) as u8;
                let i_i_h  = ((i_int / 100) % 10) as u8;
                let i_i_t  = ((i_int / 10) % 10) as u8;
                let i_i_o  = (i_int % 10) as u8;
                let i_f_o  = i_frac1 as u8;

                // 電力: p_uw [µW] -> 00000.0 mW（5桁整数＋1桁小数）
                let p_uw_u32: u32 = if p_uw > 0 { p_uw as u32 } else { 0 };
                let p_mwx10: u32 = p_uw_u32 / 100; // mW×10
                let p_int: u32 = p_mwx10 / 10; // 0..99999
                let p_frac1: u32 = p_mwx10 % 10; // 0..9
                let p_i_tth = ((p_int / 10_000) % 10) as u8;
                let p_i_th  = ((p_int / 1000) % 10) as u8;
                let p_i_h   = ((p_int / 100) % 10) as u8;
                let p_i_t   = ((p_int / 10) % 10) as u8;
                let p_i_o   = (p_int % 10) as u8;
                let p_f_o   = p_frac1 as u8;

                // 積算用の現在電力（µW）と V/I（時間重み用）を更新
                last_p_uw = p_uw as i64;
                last_v_mv = v_mv as i32;
                last_i_ua = i_ua as i32;
                // 表示は「1秒ごと、整数秒」。その秒にデータが取得できなければ出力しない。
                if curr_sec > last_printed_sec && curr_sec > 0 {
                    // 表示用の値を整数演算で作る（切り捨て）。負値は0として扱う。
                    let e_uWms_pos: u128 = if energy_uWms > 0 { energy_uWms as u128 } else { 0 };
                    // mWh（×100, 切り捨て）: µW・ms / 3.6e9
                    let mwh_x100: u128 = e_uWms_pos.saturating_mul(100)
                        / 3_600_000_000u128;
                    let mwh_int: u64 = (mwh_x100 / 100) as u64;
                    let mwh_frac2: u8 = (mwh_x100 % 100) as u8;
                    let mwh_int_cap = core::cmp::min(mwh_int, 99_999);
                    let mwh_d5 = ((mwh_int_cap / 10_000) % 10) as u8;
                    let mwh_d4 = ((mwh_int_cap / 1000) % 10) as u8;
                    let mwh_d3 = ((mwh_int_cap / 100) % 10) as u8;
                    let mwh_d2 = ((mwh_int_cap / 10) % 10) as u8;
                    let mwh_d1 = (mwh_int_cap % 10) as u8;
                    let mwh_f1: char = (b'0' + (mwh_frac2 / 10)) as char;
                    let mwh_f2: char = (b'0' + (mwh_frac2 % 10)) as char;

                    // Wh（×100, 切り捨て）: µW・ms / 3.6e12
                    let wh_x100: u128 = e_uWms_pos.saturating_mul(100)
                        / 3_600_000_000_000u128;
                    let wh_int: u64 = (wh_x100 / 100) as u64;
                    let wh_frac2: u8 = (wh_x100 % 100) as u8;
                    let wh_int_cap = core::cmp::min(wh_int, 999);
                    let wh_d3 = ((wh_int_cap / 100) % 10) as u8;
                    let wh_d2 = ((wh_int_cap / 10) % 10) as u8;
                    let wh_d1 = (wh_int_cap % 10) as u8;
                    let wh_f1: char = (b'0' + (wh_frac2 / 10)) as char;
                    let wh_f2: char = (b'0' + (wh_frac2 % 10)) as char;

                    // 単三電池（2.5 Wh = 9e12 µW・ms）に対する割合（% ×100, 切り捨て）
                    let aa_cap_uWms: u128 = 9_000_000_000_000u128;
                    let pct_x100: u128 = e_uWms_pos
                        .saturating_mul(10_000) // 100% × 100（小数2桁）
                        / aa_cap_uWms;
                    let pct_int: u64 = (pct_x100 / 100) as u64;
                    let pct_frac2: u8 = (pct_x100 % 100) as u8;
                    let pct_int_cap = core::cmp::min(pct_int, 999);
                    let pct_d3 = ((pct_int_cap / 100) % 10) as u8;
                    let pct_d2 = ((pct_int_cap / 10) % 10) as u8;
                    let pct_d1 = (pct_int_cap % 10) as u8;
                    let pct_f1: char = (b'0' + (pct_frac2 / 10)) as char;
                    let pct_f2: char = (b'0' + (pct_frac2 % 10)) as char;

                    // 時間（00時間00分00秒）— 2桁固定
                    let h_total = core::cmp::min(curr_sec / 3600, 99);
                    let m_total = (curr_sec % 3600) / 60;
                    let s_total = curr_sec % 60;
                    let h_t = ((h_total / 10) % 10) as u8;
                    let h_o = (h_total % 10) as u8;
                    let m_t = ((m_total / 10) % 10) as u8;
                    let m_o = (m_total % 10) as u8;
                    let s_t = ((s_total / 10) % 10) as u8;
                    let s_o = (s_total % 10) as u8;

                    info!(
                        "{=char}{=char}時間{=char}{=char}分{=char}{=char}秒  E={=char}{=char}{=char}{=char}{=char}.{=char}{=char} mWh ({=char}{=char}{=char}.{=char}{=char} Wh)  |  V={=char}{=char}.{=char}{=char}{=char} V  I={=char}{=char}{=char}{=char}.{=char} mA  P={=char}{=char}{=char}{=char}{=char}.{=char} mW  |  AA={=char}{=char}{=char}.{=char}{=char}%",
                        // 時刻 HH:MM:SS
                        (b'0' + h_t) as char, (b'0' + h_o) as char,
                        (b'0' + m_t) as char, (b'0' + m_o) as char,
                        (b'0' + s_t) as char, (b'0' + s_o) as char,
                        // E mWh 5桁.2桁
                        (b'0' + mwh_d5) as char, (b'0' + mwh_d4) as char,
                        (b'0' + mwh_d3) as char, (b'0' + mwh_d2) as char,
                        (b'0' + mwh_d1) as char, mwh_f1, mwh_f2,
                        // E Wh 3桁.2桁
                        (b'0' + wh_d3) as char, (b'0' + wh_d2) as char,
                        (b'0' + wh_d1) as char, wh_f1, wh_f2,
                        // V 2桁.3桁
                        (b'0' + v_i_t) as char, (b'0' + v_i_o) as char,
                        (b'0' + v_f_h) as char, (b'0' + v_f_t) as char,
                        (b'0' + v_f_o) as char,
                        // I 4桁.1桁
                        (b'0' + i_i_th) as char, (b'0' + i_i_h) as char,
                        (b'0' + i_i_t) as char, (b'0' + i_i_o) as char,
                        (b'0' + i_f_o) as char,
                        // P 5桁.1桁
                        (b'0' + p_i_tth) as char, (b'0' + p_i_th) as char,
                        (b'0' + p_i_h) as char, (b'0' + p_i_t) as char,
                        (b'0' + p_i_o) as char, (b'0' + p_f_o) as char,
                        // AA % 3桁.2桁
                        (b'0' + pct_d3) as char, (b'0' + pct_d2) as char,
                        (b'0' + pct_d1) as char, pct_f1, pct_f2
                    );
                    last_printed_sec = curr_sec;
                }
            }
            Ok(None) => {
                // 新規データ未到来。次サイクルへ。
            }
            Err(_e) => {
                warn!("INA219 read error");
            }
        }

        // 周期待ち（目安 500ms）
        timer.delay_ms(LOOP_MS);
    }
}

/// INA219 の初期化（校正 + 連続測定設定）
/// 使用アドレスは `INA_ADDR`
fn init_ina219<I2CIF>(i2c: I2CIF) -> Result<ina::SyncIna219<I2CIF, IntCalibration>, ()>
where
    I2CIF: embedded_hal::i2c::I2c,
{
    info!("init: calc calibration...");

    // 校正（IntCalibration）: current_LSB[µA/bit] は MAX_EXPECTED_AMPS / 2^15 で見積
    let current_lsb_ua_per_bit = (MAX_EXPECTED_AMPS * 1_000_000.0 / 32768.0) as i64;
    let r_shunt_uohm = (SHUNT_OHMS * 1_000_000.0) as u32;

    info!("  current_lsb_ua_per_bit = {=i64}", current_lsb_ua_per_bit);
    info!("  r_shunt_uohm          = {=u32}", r_shunt_uohm);

    let calib = match IntCalibration::new(
        ina::calibration::MicroAmpere(current_lsb_ua_per_bit),
        r_shunt_uohm,
    ) {
        Some(c) => c,
        None => {
            error!("init: failed to create calibration");
            return Err(());
        }
    };

    // 設定：32Vレンジ / シャント±320mV（最大ゲイン）
    let cfg = Configuration {
        bus_voltage_range: BusVoltageRange::Fsr32v,
        shunt_voltage_range: ShuntVoltageRange::Fsr320mv,
        ..Default::default()
    };

    // 使用アドレスは `INA_ADDR`
    let address = match Address::from_byte(INA_ADDR) {
        Ok(a) => a,
        Err(_) => {
            error!("Invalid INA219 address: 0x{=u8:x}", INA_ADDR);
            return Err(());
        }
    };

    info!("init at address 0x{=u8:x}...", INA_ADDR);
    let mut dev = match ina::SyncIna219::new_calibrated(i2c, address, calib) {
        Ok(d) => d,
        Err(e) => {
            match e.reason {
                InitializationErrorReason::I2cError(_) => error!("  I2C error"),
                InitializationErrorReason::ConfigurationNotDefaultAfterReset => {
                    error!("  cfg not default after reset")
                }
                InitializationErrorReason::RegisterNotZeroAfterReset(_) => {
                    error!("  reg not zero after reset")
                }
                InitializationErrorReason::ShuntVoltageOutOfRange => {
                    error!("  shunt voltage out of range")
                }
                InitializationErrorReason::BusVoltageOutOfRange => {
                    error!("  bus voltage out of range")
                }
            }
            return Err(());
        }
    };

    dev.set_configuration(cfg).map_err(|_| ())?;
    info!("INA219 initialized at 0x{=u8:x}", INA_ADDR);
    Ok(dev)
}

/// 1サイクル分の計測値取得（mV, µA, µW）
fn ina_next<I2CIF>(
    dev: &mut ina::SyncIna219<I2CIF, IntCalibration>,
) -> Result<Option<(i32, i32, i32)>, ()>
where
    I2CIF: embedded_hal::i2c::I2c,
{
    // next_measurement(): Ok(Some(..)) のときのみ新データ
    match dev.next_measurement().map_err(|_| ())? {
        Some(m) => {
            // 期待する単位：Bus電圧[mV] / 電流[µA] / 電力[µW]
            let v_mv = m.bus_voltage.voltage_mv() as i32;
            let i_ua = m.current.0 as i32;
            let p_uw = m.power.0 as i32;
            Ok(Some((v_mv, i_ua, p_uw)))
        }
        None => Ok(None),
    }
}

// BOOT2（必須）
#[link_section = ".boot2"]
#[used]
static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;
