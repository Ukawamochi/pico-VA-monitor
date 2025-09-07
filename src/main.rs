#![no_std]
#![no_main]

// use core::fmt::Write as _;

use cortex_m_rt::entry;
use defmt::*;
use defmt_rtt as _;
// use fugit::ExtU32 as _;
use fugit::RateExtU32 as _;
use panic_probe as _;

use rp2040_hal as hal;
use hal::{clocks::init_clocks_and_plls, gpio::FunctionI2C, pac, sio::Sio, watchdog::Watchdog, I2C, Timer};
use rp2040_hal::Clock;
use embedded_hal::delay::DelayNs;

// ドライバ（sync API）
// 注: 実クレートのAPI名に合わせて適宜更新してください
use ina219 as ina;
use ina219::address::Address;
use ina219::calibration::IntCalibration;
use ina219::configuration::{Configuration, BusVoltageRange, ShuntVoltageRange};
use ina219::errors::InitializationErrorReason;

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
// INA219 のI2Cアドレス（固定）
// A1のみジャンパで VCC の場合は 0x44 が一般的。ブレークアウトによっては 0x48（A1=SDA）, 0x4C（A1=SCL）, 既定 0x40（A1=GND）の場合も。
// 変更したい場合は下記定数を書き換えてください。
const INA_ADDR: u8 = 0x40;

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

    // I2C0 @ 400kHz
    // I2Cピンはプルアップが必須。PullUp入力→I2C機能へ遷移させると型が PullUp になります。
    let sda = pins.gpio4.into_pull_up_input().into_function::<FunctionI2C>();
    let scl = pins.gpio5.into_pull_up_input().into_function::<FunctionI2C>();
    let i2c = I2C::i2c0(
        pac.I2C0,
        sda,
        scl,
        // I2C クロック。配線条件によっては 400kHz で不安定になる場合があるため、さらに 50kHz に下げて安定性を優先。
        50.kHz(),
        &mut pac.RESETS,
        // system_clock 周波数を渡す（I2C タイミング計算に使用）。
        // rp2040-hal の例と同様に system_clock を指定するのが正。
        clocks.system_clock.freq(),
    );

    // タイマ（Δt計測 & ウェイト）
    let mut timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);
    // RTT アタッチ猶予（ホストが接続する時間を与える）
    timer.delay_ms(500u32);
    info!("=== PICO VA MONITOR BOOT ===");
    info!("System initialized, starting INA219...");

    // INA219 初期化（デバッグ情報追加）
    info!("Starting INA219 initialization...");
    
    let mut ina = match init_ina219(i2c) {
        Ok(device) => {
            info!("INA219 initialization successful - device ready");
            device
        }
        Err(_) => {
            error!("INA219 initialization failed - check I2C wiring and address");
            error!("Expected connections:");
            error!("  VCC -> Pico 3V3");
            error!("  GND -> Pico GND");
            error!("  SDA -> Pico GPIO4");
            error!("  SCL -> Pico GPIO5");
            error!("  VIN+/VIN- -> measurement circuit");
            core::panic!("INA219 init failed")
        }
    };

    // 統計・積算
    let mut rs_v = metrics::RunningStats::new();
    let mut rs_i = metrics::RunningStats::new();
    let mut rs_p = metrics::RunningStats::new();
    let mut acc = metrics::Accumulators::new(1); // 1 mA 未満を0扱い
    info!("Metrics and accumulators initialized");

    // バッファ（ASCIIバー）
    let mut bar_v = [0u8; termviz::BAR_W];
    let mut bar_i = [0u8; termviz::BAR_W];
    let mut bar_p = [0u8; termviz::BAR_W];
    info!("Display buffers initialized");

    // ループ
    info!("=== STARTING MAIN MEASUREMENT LOOP ===");
    let mut last = timer.get_counter();
    let mut cyc: u32 = 0;
    loop {
        let now = timer.get_counter();
        let dt_ms: u32 = (now - last).to_millis() as u32;
        last = now;

        match ina_next(&mut ina) {
            Ok(Some((v_mv, i_ua, p_uw))) => {
                // 最初の測定成功時にログ出力（一度だけ）
                if cyc == 0 {
                    info!("First measurement successful: V={=i32}mV, I={=i32}uA, P={=i32}uW", v_mv, i_ua, p_uw);
                }
                
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
                info!("V {=f32} V [{=str}] {=u8}%", v_v, v_bar, v_pct);
                info!(
                    "I {=f32} mA [{=str}] {=u8}%   P {=f32} mW [{=str}] {=u8}%",
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
fn init_ina219<I2CIF>(i2c: I2CIF) -> Result<ina::SyncIna219<I2CIF, IntCalibration>, ()>
where
    I2CIF: embedded_hal::i2c::I2c,
{
    info!("init_ina219: Starting calibration calculation...");
    
    // 校正（IntCalibration）: current_LSB[µA/bit] は MAX_EXPECTED_AMPS / 2^15 で見積
    let current_lsb_ua_per_bit = (MAX_EXPECTED_AMPS * 1_000_000.0 / 32768.0) as i64;
    let r_shunt_uohm = (SHUNT_OHMS * 1_000_000.0) as u32;
    
    info!("init_ina219: current_lsb_ua_per_bit = {=i64}", current_lsb_ua_per_bit);
    info!("init_ina219: r_shunt_uohm = {=u32}", r_shunt_uohm);
    
    let calib = match IntCalibration::new(ina::calibration::MicroAmpere(current_lsb_ua_per_bit), r_shunt_uohm) {
        Some(c) => {
            info!("init_ina219: Calibration created successfully");
            c
        }
        None => {
            error!("init_ina219: Failed to create calibration");
            return Err(());
        }
    };

    // 初期化＋校正適用
    // 設定：32Vレンジ / シャント320mV（最大ゲイン）
    let cfg = Configuration {
        bus_voltage_range: BusVoltageRange::Fsr32v,
        shunt_voltage_range: ShuntVoltageRange::Fsr320mv,
        ..Default::default()
    };
    
    // 固定アドレスで初期化（ユーザーは INA_ADDR を変更してください）
    let addr = INA_ADDR;
    info!("init_ina219: Using fixed address 0x{=u8:x}", addr);
    match ina::SyncIna219::new_calibrated(i2c, Address::from_byte(addr).unwrap(), calib) {
        Ok(mut dev) => {
            info!("init_ina219: Device created, writing configuration...");
            dev.set_configuration(cfg).map_err(|_| ())?;
            info!("INA219 initialized at 0x{=u8:x}", addr);
            Ok(dev)
        }
        Err(e) => {
            error!("init_ina219: Initialization failed at address 0x{=u8:x}", addr);
            match e.reason {
                InitializationErrorReason::I2cError(_) => error!("  Reason: I2C communication error"),
                InitializationErrorReason::ConfigurationNotDefaultAfterReset => error!("  Reason: Configuration not default after reset"),
                InitializationErrorReason::RegisterNotZeroAfterReset(_) => error!("  Reason: Register not zero after reset"),
                InitializationErrorReason::ShuntVoltageOutOfRange => error!("  Reason: Shunt voltage out of range"),
                InitializationErrorReason::BusVoltageOutOfRange => error!("  Reason: Bus voltage out of range"),
            }
            error!("  Check wiring and power supply");
            Err(())
        }
    }
}

/// 1サイクル分の計測値取得（mV, µA, µW）
fn ina_next<I2CIF>(dev: &mut ina::SyncIna219<I2CIF, IntCalibration>) -> Result<Option<(i32, i32, i32)>, ()>
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
