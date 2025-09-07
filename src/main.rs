#![no_std]
#![no_main]

// 本ファイルは「最小限の機能」に絞った版です。
// 目的: INA219 の電圧/電流/電力を 500ms 間隔で defmt に出すだけ。
// メトリクス集計やASCIIバーは一旦外しています（確実な動作優先）。

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
const INA_ADDR: u8 = 0x44; // INA219のデフォルトアドレス:0x40（固定）

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

    // INA219 初期化（固定アドレス 0x40）
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
    let mut last = timer.get_counter();
    loop {
        let now = timer.get_counter();
        let _dt_ms: u32 = (now - last).to_millis() as u32;
        last = now;

        match ina_next(&mut ina) {
            Ok(Some((v_mv, i_ua, p_uw))) => {
                // 単位変換して1行だけ出す
                let v_v = v_mv as f32 / 1000.0;
                let i_ma = i_ua as f32 / 1000.0;
                let p_mw = p_uw as f32 / 1000.0;
                info!("V={=f32} V  I={=f32} mA  P={=f32} mW", v_v, i_ma, p_mw);
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
/// 固定アドレス 0x40 を使用
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

    // 固定アドレス 0x40 を使用
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
