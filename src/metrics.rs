//! 統計・積算ロジック（no_std）
//! - 逐次統計（Welford法）: RunningStats
//! - 積算（固定小数）: Accumulators（電荷[µA·s]、エネルギー[µW·s]、稼働時間[ms]）

#![allow(dead_code)]



/// 逐次統計（Welford法）
/// 平均・分散・標準偏差・最小・最大を保持
#[derive(Clone, Copy, Default)]
pub struct RunningStats {
    pub n: u64,
    pub mean: f32,
    m2: f32,
    pub min: f32,
    pub max: f32,
}

impl RunningStats {
    /// 新規作成
    pub const fn new() -> Self {
        Self { n: 0, mean: 0.0, m2: 0.0, min: f32::INFINITY, max: f32::NEG_INFINITY }
    }

    /// 値を追加入力
    pub fn update(&mut self, x: f32) {
        self.n += 1;
        let n_f = self.n as f32;
        let delta = x - self.mean;
        self.mean += delta / n_f;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
        if x < self.min { self.min = x; }
        if x > self.max { self.max = x; }
    }

    /// 標本分散
    pub fn variance(&self) -> f32 {
        if self.n < 2 { 0.0 } else { self.m2 / (self.n as f32 - 1.0) }
    }

    /// 標準偏差
    pub fn stddev(&self) -> f32 { libm::sqrtf(self.variance()) }
}

/// 積算器（固定小数）：
/// - 累計電荷: µA·s（u128）
/// - 累計エネルギー: µW·s（u128）
/// - 稼働時間: ms（u64）
pub struct Accumulators {
    charge_uas: u128,
    energy_uws: u128,
    pub uptime_ms: u64,
    /// 微小電流のカットオフ（mA）。|I| < cutoff の場合0扱い
    pub current_cutoff_ma: u32,
}

impl Accumulators {
    pub const fn new(cutoff_ma: u32) -> Self {
        Self { charge_uas: 0, energy_uws: 0, uptime_ms: 0, current_cutoff_ma: cutoff_ma }
    }

    /// 積算更新
    /// v_v: V, i_ma: mA, p_mw: mW, dt_ms: 経過時間[ms]
    pub fn update(&mut self, _v_v: f32, i_ma: f32, p_mw: f32, dt_ms: u32) {
        self.uptime_ms = self.uptime_ms.saturating_add(dt_ms as u64);

        // 微小電流カットオフ
        let i_ma_eff = if i_ma.abs() < self.current_cutoff_ma as f32 { 0.0 } else { i_ma };

        // 電荷: µA·s = (i[mA]*1000)[µA] * (dt[ms]/1000)[s]
        //      = i[mA] * dt[ms]
        // 単位合わせ：i[mA]*dt[ms] = (mA·ms) = µA·s
        let dq_uas = (i_ma_eff as f64) * (dt_ms as f64);
        if dq_uas.is_finite() && dq_uas >= 0.0 {
            self.charge_uas = self.charge_uas.saturating_add(dq_uas as u128);
        }

        // エネルギー: µW·s = (p[mW]*1000)[µW] * (dt[ms]/1000)[s]
        //           = p[mW] * dt[ms]
        let de_uws = (p_mw as f64) * (dt_ms as f64);
        if de_uws.is_finite() && de_uws >= 0.0 {
            self.energy_uws = self.energy_uws.saturating_add(de_uws as u128);
        }
    }

    /// 累計電荷の読み出し（mAh）
    pub fn readout_charge_mah(&self) -> f32 {
        // 1 mAh = 3_600_000 µA·s
        (self.charge_uas as f64 / 3_600_000.0) as f32
    }

    /// 累計エネルギーの読み出し（mWh, Wh）
    pub fn readout_energy(&self) -> (f32, f32) {
        // 1 mWh = 3_600_000 µW·s, 1 Wh = 1000 mWh
        let mwh = (self.energy_uws as f64 / 3_600_000.0) as f32;
        let wh = mwh / 1000.0;
        (mwh, wh)
    }
}

/// 電池本数換算（AA/AAA）。E_Wh / 代表容量[Wh]
pub fn battery_equiv(wh: f32, e_aa_wh: f32, e_aaa_wh: f32) -> (f32, f32) {
    let aa = if e_aa_wh > 0.0 { wh / e_aa_wh } else { 0.0 };
    let aaa = if e_aaa_wh > 0.0 { wh / e_aaa_wh } else { 0.0 };
    (aa, aaa)
}
