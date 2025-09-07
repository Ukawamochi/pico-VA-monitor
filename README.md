# pico-va-monitor（RP2040 × INA219）

Raspberry Pi Pico / Pico W（RP2040）と INA219 による**電圧・電流・電力**モニタです。500 ms 周期で `defmt` ログに**ASCIIバー**付きの即時値を出力し、定期的に**累計（mAh / mWh/Wh）**と**電池本数換算（AA/AAA）**、**統計（平均/最小/最大/標準偏差）**を表示します。

- HAL: `rp2040-hal`（安定版）
- ログ: `defmt` + `defmt-rtt`
- パニック: `panic-probe`（`print-defmt`有効）
- I²C: 400 kHz（I2C0 / GPIO4,5）
- 計測周期: 500 ms（実測Δtで積分）
- 依存: `embedded-hal`, `ina219`（sync機能）, `fugit`

## 配線（Pico ピン表記）

- I2C0 SDA = GPIO4（ピン6）
- I2C0 SCL = GPIO5（ピン7）
- 3V3 ↔ VCC、GND ↔ GND
- シャント抵抗例: **0.1 Ω**（ブレークアウト基板の一般的既定）
- 被測定回路の向き: **VIN+ が電源側、VIN− が負荷側**

### 初心者向け: 3V3 と VCC の意味

- 3V3（3.3V）は Raspberry Pi Pico の電源出力ピンの一つで、Pico 本体のロジック電圧も 3.3V です。
- VCC は部品（今回は INA219 ブレイクアウト）の電源入力を指す一般的な呼び名です。多くのブレイクアウト基板は VCC と表記されています。
- INA219 ブレイクアウトは 2.7V〜5.5V で動作しますが、基板上の I2C プルアップは VCC に接続されている場合が多いので、Pico と直結するなら VCC に **3V3（Pico の 3.3V 出力）を接続してください。**
- もし VCC を 5V にするとブレイクアウトの SDA/SCL に 5V のプルアップがかかり、Pico の I/O を壊す可能性があります（レベル変換がない場合）。

### 注意: VIN+ / VIN− の使い方

- VIN+ / VIN− は被測定回路の電流を測るための入力で、通常は被測定回路の「電源側」と「負荷側」を直列に接続します。
- VIN+ は電源側（例: バッテリの＋）、VIN− は負荷側（例: モータや回路の＋）に接続します。GND は必ず Pico と共通にしてください。VIN+/VIN− は INA219 の測定経路で、ここに電源を入れると基板上のシャント抵抗を経由して電流が測れます。

## ビルド

```bash
rustup target add thumbv6m-none-eabi
cargo build --release
```

`DEFMT_LOG` の推奨設定（VSCode 例）:

```jsonc
// .vscode/tasks.json
{
  "label": "cargo build (thumbv6m)",
  "options": { "env": { "DEFMT_LOG": "info" } }
}
// .vscode/launch.json
{
  "env": { "DEFMT_LOG": "info" },
  "coreConfigs": [{ "rttEnabled": true, "rttChannelFormats": [{"dataFormat": "Defmt"}] }]
}
```

## 書き込み・実行例

- probe-rs（RTT で defmt 確認）
  ```bash
  probe-rs run --chip RP2040 --release
  ```
- UF2（BOOTSEL ドラッグ&ドロップ）
  ```bash
  elf2uf2-rs target/thumbv6m-none-eabi/release/pico-va-monitor
  # 生成された .uf2 を RPI-RP2 にコピー
  ```

## 調整可能な定数

- `src/main.rs`
  - `SHUNT_OHMS`（シャント抵抗 [Ω]、例: 0.1）
  - `MAX_EXPECTED_AMPS`（最大期待電流 [A]、例: 2.0）
  - `V_MAX / I_MAX / P_MAX`（ASCIIメータのスケール）
- `src/metrics.rs`
  - カットオフ電流（`Accumulators::new(1)` の引数 [mA]）

校正は `ina219::IntCalibration` を使用し、`SHUNT_OHMS` と `MAX_EXPECTED_AMPS` から `current_LSB`（µA/bit）を算出して適用します。

## INA219 の I2C アドレスを変える方法（ハード側 / ソフト側）

1) ハード側（ブレイクアウトの A0 / A1 ジャンパ）

- 多くの INA219 ブレイクアウト（Adafruit 等）には A0 と A1 のサーマブルジャンパ（またはハンダパッド）があり、これを GND / VCC / SDA / SCL に接続してアドレスを切り替えます。
- Adafruit の一般的なマッピング（参考）:
  - デフォルト（A0=GND, A1=GND） = 0x40
  - A0 を VCC に接続（A0=VCC, A1=GND） = 0x41
  - A1 を VCC に接続（A0=GND, A1=VCC） = 0x44
  - A0 と A1 を両方 VCC にすると = 0x45

  （ブレイクアウトのドキュメントを必ず確認してください。ボードによっては接続先のラベルが異なります。）

2) ソフト側（このリポジトリの変更箇所）

- コード内では `src/main.rs` の `init_ina219` 関数で INA219 を生成しています。現在は既定アドレス（0x40）を使うように `ina::Address::default()` が使われています。
- もしハードでアドレスを変更したら、ソフト側でも同じアドレスを指定してください。例をいくつか示します（このリポジトリのスタイルに合わせて `ina` プレフィックスを使用）：

  - デフォルト（そのまま）

```rust
// そのまま既定（A0=A1=GND -> 0x40）
let mut dev = ina::SyncIna219::new(i2c, ina::Address::default());
```

  - バイトで指定する（例: 0x41）

```rust
let addr = ina::Address::from_byte(0x41).expect("invalid INA219 address");
let mut dev = ina::SyncIna219::new(i2c, addr);
```

  - ピン指定で作る（より明示的）

```rust
// a0 = Vcc, a1 = Gnd -> 0x41
let addr = ina::address::Address::from_pins(ina::address::Pin::Vcc, ina::address::Pin::Gnd);
let mut dev = ina::SyncIna219::new(i2c, addr);
```

- 変更箇所は `src/main.rs` の `init_ina219` 内の次の行です（置き換えてください）:

```rust
// 変更前
let mut dev = ina::SyncIna219::new(i2c, ina::Address::default());

// 変更後（例: 0x41 を使う場合）
let addr = ina::Address::from_byte(0x41).unwrap();
let mut dev = ina::SyncIna219::new(i2c, addr);
```

以上で、ハードのジャンパ設定とソフトのアドレス指定が一致するようにしてください。

## 表示例（defmt）

```
V  5.02 V [=============================..] 94%
I  128.7 mA [#######......................] 22%   P  646.5 mW [#############..............] 40%
Q=12.345 mAh  E=62.500 mWh (AA≈0.03本 / AAA≈0.06本)  up=00:07:12  I(avg/min/max/std)=135.2/0.0/412.8/45.1 mA
```

## 電池本数換算の前提

- AA: 代表値 ≈ **2.5 Wh**
- AAA: 代表値 ≈ **1.1 Wh**

実容量はメーカー・負荷・温度依存で変動します。目安表示としてご利用ください。

## ノイズ・安定化のヒント

- 平均回数（`Resolution::Avg128` 以上）と変換時間を適切に設定
- PGA（`ShuntVoltageRange`）を用途に合わせて選択（大電流で飽和しない設定）
- 配線を短くし、GND リターンを共有しすぎない
- 微小電流カットオフを適宜調整（積算の誤差抑制）

## 永続化について

フラッシュ寿命の観点から、本初期版では累計のフラッシュ保存は未対応です。長期ログの永続化が必要な場合は外部 **FRAM** の利用や、ホスト側での収集を推奨します。

## 既知の注意

- `ina219` クレートの API 名称（`SyncIna219`, `IntCalibration`, `next_measurement()` 等）は利用バージョンにより差異がある場合があります。最新版に合わせて `Cargo.toml` のバージョンを調整してください。
- `DEFMT_LOG` が未設定だとログが表示されないことがあります。`info` 以上を推奨します。
- フラッシュエラー時は `programBinary` パスや `memory.x` の設定を確認してください。

## ライセンス

本リポジトリにライセンスは含めていません。必要に応じて追加してください。

***

開発メモ：コード内コメント・ドキュメントは日本語で統一しています。`cargo fmt --all` と `cargo clippy -D warnings` を通すことを推奨します。

