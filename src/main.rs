//! RP2040 + INA219 電圧・電流・電力モニタ（500ms周期 / 統計・積算・ASCIIバー表示）
//!
//! 配線（Pico ピン）:
//! - I2C0 SDA = GPIO4（ピン6）
//! - I2C0 SCL = GPIO5（ピン7）
//! - 3V3 ↔ VCC, GND ↔ GND
//! - シャント: 0.1Ω を想定（VIN+ が電源側、VIN− が負荷側）
//!
//! Runner 例（コメント）:
//! - probe-rs: `probe-rs run --chip RP2040 --release`
//! - UF2: `elf2uf2-rs target/thumbv6m-none-eabi/release/pico-va-monitor` を BOOTSEL でコピー

#![no_std]
#![no_main]

use core::fmt::Write as _;

use cortex_m_rt::entry;
use defmt::*;
use defmt_rtt as _;
use fugit::ExtU32 as _;
use fugit::RateExtU32 as _;
use panic_probe as _;

use rp2040_hal as hal;
use hal::{clocks::init_clocks_and_plls, gpio::FunctionI2C, pac, sio::Sio, watchdog::Watchdog, I2C, Timer};

// ドライバ（sync API）
// 注: 実クレートのAPI名に合わせて適宜更新してください
use ina219 as ina;

mod metrics;
mod termviz;

// ---- 設定定数 ----
const SHUNT_OHMS: f32 = 0.1; // シャント抵抗 [Ω]
const MAX_EXPECTED_AMPS: f32 = 2.0; // 最大期待電流 [A]

const V_MAX: f32 = 5.5; // Vbus 表示上限 [V]
const I_MAX: f32 = MAX_EXPECTED_AMPS * 1000.0; // I 表示上限 [mA]
const P_MAX: f32 = V_MAX * I_MAX; // P 表示上限 [mW]（近似）

const E_AA_WH: f32 = 2.5;  // AA の代表的エネルギー [Wh]
const E_AAA_WH: f32 = 1.1; // AAA の代表的エネルギー [Wh]

const LOOP_MS: u32 = 500; // 計測周期 [ms]

#[entry]
fn main() -> ! {
    info!("pico-va-monitor start");

    // PAC 取得
    let mut pac = pac::Peripherals::take().unwrap();
    let core = pac::CorePeripherals::take().unwrap();
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

    // I2C0 @ 400kHz
    let sda = pins.gpio4.into_mode::<FunctionI2C>();
    let scl = pins.gpio5.into_mode::<FunctionI2C>();
    let i2c = I2C::i2c0(
        pac.I2C0,
        sda,
        scl,
        400.kHz(),
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
    );

    // タイマ（Δt計測 & ウェイト）
    let mut timer = Timer::new(pac.TIMER, &mut pac.RESETS);

    // INA219 初期化
    let mut ina = init_ina219(i2c).unwrap_or_else(|_| panic!("INA219 init failed"));

    // 統計・積算
    let mut rs_v = metrics::RunningStats::new();
    let mut rs_i = metrics::RunningStats::new();
    let mut rs_p = metrics::RunningStats::new();
    let mut acc = metrics::Accumulators::new(1); // 1 mA 未満を0扱い

    // バッファ（ASCIIバー）
    let mut bar_v = [0u8; termviz::BAR_W];
    let mut bar_i = [0u8; termviz::BAR_W];
    let mut bar_p = [0u8; termviz::BAR_W];

    // ループ
    let mut last = timer.get_counter();
    let mut cyc: u32 = 0;
    loop {
        let now = timer.get_counter();
        let dt_ms: u32 = (now - last).to_millis() as u32;
        last = now;

        match ina_next(&mut ina) {
            Ok((v_mv, i_ua, p_uw)) => {
                // 単位変換
                let v_v = v_mv as f32 / 1000.0;
                let i_ma = i_ua as f32 / 1000.0;
                let p_mw = p_uw as f32 / 1000.0;

                // 統計・積算
                acc.update(v_v, i_ma, p_mw, dt_ms.max(1));
                rs_v.update(v_v);
                rs_i.update(i_ma);
                rs_p.update(p_mw);

                // 即時量（バー）
                let v_pct = termviz::pct(v_v, V_MAX);
                let i_pct = termviz::pct(i_ma.max(0.0), I_MAX);
                let p_pct = termviz::pct(p_mw.max(0.0), P_MAX);
                let v_bar = termviz::render_bar(v_pct, &mut bar_v);
                let i_bar = termviz::render_bar(i_pct, &mut bar_i);
                let p_bar = termviz::render_bar(p_pct, &mut bar_p);

                // defmt 出力（1～2行）
                info!("V {=f32} V [{:str}] {=u8}%", v_v, v_bar, v_pct);
                info!(
                    "I {=f32} mA [{:str}] {=u8}%   P {=f32} mW [{:str}] {=u8}%",
                    i_ma, i_bar, i_pct, p_mw, p_bar, p_pct
                );

                // 周期要約（2～5秒ごと）
                cyc = cyc.wrapping_add(1);
                if cyc % 4 == 0 {
                    let (mwh, wh) = acc.readout_energy();
                    let mah = acc.readout_charge_mah();
                    let (aa_eq, aaa_eq) = metrics::battery_equiv(wh, E_AA_WH, E_AAA_WH);
                    let up_s = acc.uptime_ms / 1000;
                    let h = up_s / 3600;
                    let m = (up_s % 3600) / 60;
                    let s = up_s % 60;
                    info!(
                        "Q={=f32} mAh  E={=f32} mWh ({=f32} Wh) (AA≈{=f32}本 / AAA≈{=f32}本)  up={=u32}:{=u32}:{=u32}",
                        mah, mwh, wh, aa_eq, aaa_eq, h, m, s
                    );
                    info!(
                        "I(avg/min/max/std)={=f32}/{=f32}/{=f32}/{=f32} mA",
                        rs_i.mean, rs_i.min, rs_i.max, rs_i.stddev()
                    );
                }
            }
            Err(e) => {
                warn!("INA219 read error");
            }
        }

        // 周期待ち（目安 500ms）
        timer.delay_ms(LOOP_MS);
    }
}

/// INA219 の初期化（校正 + 連続測定設定）
fn init_ina219<I2CIF>(i2c: I2CIF) -> Result<ina::SyncIna219<I2CIF>, ()>
where
    I2CIF: embedded_hal::blocking::i2c::Write + embedded_hal::blocking::i2c::WriteRead,
{
    // アドレス既定 0x40
    let mut dev = ina::SyncIna219::new(i2c, ina::Address::default());

    // 設定例：32Vレンジ / ゲインDiv8（用途に合わせて調整）
    let cfg = ina::Config {
        bus_range: ina::BusVoltageRange::Range32V,
        shunt_gain: ina::ShuntVoltageRange::GainDiv8,
        bus_adc: ina::AdcResolution::Avg128,
        shunt_adc: ina::AdcResolution::Avg128,
        mode: ina::OperatingMode::ShuntAndBusContinuous,
    };
    dev.set_config(&cfg).map_err(|_| ())?;

    // 校正（IntCalibration）
    // current_LSB[µA/bit] は MAX_EXPECTED_AMPS を 2^15 で割って見積
    let current_lsb_ua_per_bit = (MAX_EXPECTED_AMPS * 1_000_000.0 / 32768.0) as u32;
    let r_shunt_uohm = (SHUNT_OHMS * 1_000_000.0) as u32;
    let calib = ina::IntCalibration::new(r_shunt_uohm, current_lsb_ua_per_bit);
    dev.set_calibration(&calib).map_err(|_| ())?;

    Ok(dev)
}

/// 1サイクル分の計測値取得（mV, µA, µW）
fn ina_next<I2CIF>(dev: &mut ina::SyncIna219<I2CIF>) -> Result<(i32, i32, i32), ()>
where
    I2CIF: embedded_hal::blocking::i2c::Write + embedded_hal::blocking::i2c::WriteRead,
{
    // next_measurement() でまとめて取得
    let m = dev.next_measurement().map_err(|_| ())?;
    // 期待する単位：Bus電圧[mV] / 電流[µA] / 電力[µW]
    Ok((m.bus_mv, m.current_ua, m.power_uw))
}

// BOOT2（必須）
#[link_section = ".boot2"]
#[used]
static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

