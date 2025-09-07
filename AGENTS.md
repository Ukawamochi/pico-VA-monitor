# Repository Guidelines

対話とコメントアウト、ドキュメントの作成には日本語を使ってください。わたしは日本語話者です。

## 実装の流れについて
まずはじめにプロンプトの内容を正確に解釈します。考えの過程を示しながら実装の計画を立てます。そのあとは立てた開発計画に則って段階的に解決を図りましょう。

## プロジェクト構成とモジュール
- ソース: `src/main.rs` がエントリポイント（`no_std`/`no_main`）。Embassy（`embassy-executor`/`embassy-rp`/`embassy-time`）を使用し、Pico W 内蔵 LED を CYW43 経由で点滅。ログは `defmt-rtt`。
- 設定: `.cargo/config.toml` でターゲット `thumbv6m-none-eabi` とリンカ引数（`link.x`/`defmt.x`）を指定。メモリレイアウトは `memory.x`。
- パッケージ: 単一バイナリ `main`（`Cargo.toml`）。成果物は `target/`。CYW43 の FW/CLM は `cyw43-firmware/` に配置。

## defmt ログ設定
### 環境変数設定
- **ログレベル制御**: `DEFMT_LOG=info` （`trace`, `debug`, `info`, `warn`, `error` から選択）
- **設定場所**: 
  - `.vscode/tasks.json` の `env` セクション: ビルド時のログレベル指定
  - `.vscode/launch.json` の `env` セクション: デバッグ実行時のログレベル指定
  
### VSCode 設定例
```json
// .vscode/tasks.json
{
  "label": "cargo build (thumbv6m)",
  "options": {
    "env": {
      "DEFMT_LOG": "info"  // trace は詳細すぎるため info 推奨
    }
  }
}

// .vscode/launch.json  
{
  "env": {
    "DEFMT_LOG": "info"
  },
  "coreConfigs": [{
    "rttEnabled": true,
    "rttChannelFormats": [{"dataFormat": "Defmt"}]
  }]
}
```

### トラブルシューティング
- **ログが表示されない**: 環境変数 `DEFMT_LOG` の設定とRTT設定を確認
- **ログが多すぎる**: `trace` → `info` に変更して重要な情報のみ表示
- **フラッシュエラー**: `programBinary` パスとメモリ設定を確認

## ビルド・書き込み・開発コマンド
- 事前準備: `rustup target add thumbv6m-none-eabi`。ツール: `cargo install elf2uf2-rs`、必要に応じて `cargo install cargo-flash`。Linux 権限は `for-linux.md` 参照。
- ビルド: `cargo build`（または `cargo build --release`）。
- UF2 変換: `elf2uf2-rs target/thumbv6m-none-eabi/debug/main`（リリースは `release/main`）。
- 書き込み: BOOTSEL で `RPI-RP2` へ `.uf2` をコピー、または `cargo flash --chip RP2040 --release`。
- ログ（defmt/RTT）: `cargo-embed` や probe-rs RTT で `defmt` ログを確認。

## コーディング規約・命名
- 整形/静的解析: `cargo fmt --all`、`cargo clippy -D warnings` を通すこと。
- 命名: ファイル/関数は `snake_case`、型/列挙は `CamelCase`、定数は `SCREAMING_SNAKE_CASE`。
- 組込み方針: `no_std` の境界を明確にし、HAL/ドライバ依存と純粋ロジックを分離。非同期は `embassy-time` の遅延/タイマを使用。
- 言語: 今後のコード内コメント、ドキュメント注釈、コミットメッセージ、PR 説明は全て日本語で記述してください。

## テストガイドライン
- 組込み用テストは未設定。新規ロジックは `lib` 化し `#[cfg(test)]` でユニットテストを追加、ホストで `cargo test` 実行を推奨。
- あるいは `defmt-test` の導入を検討。
- 命名例: `mod foo { #[test] fn does_xxx() { ... } }`。純粋ロジックの網羅性を意識。

## コミット・プルリクエスト
- コミット: 簡潔な命令形サマリ＋背景/影響。Issue 参照例: `Fixes #12`。件名/本文は日本語を推奨。
- PR: 目的、使用ハード（ボード/プローブ）、ビルド/書込手順、`defmt` ログ/スクリーンショット、リスク/ロールバック。チェック: `cargo fmt`、`cargo clippy`、`cargo build --release`。

## セキュリティ/設定の注意
- udev などの権限設定は `for-linux.md` を参照。
- 通信プロトコルの仕様は`Spec-tx.md`を参照。
- 機密情報やプローブのシリアルはコミットしない。`memory.x` と `.cargo/config.toml` は実機に合わせて管理。
