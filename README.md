# RustとPicoを使った開発の雛形

毎回既存のプログラムからいらないものを削除して作り直すのはめんどくさいのでスターターを作った

###　特徴
- defmt::printnに対応!
- probe-rsでデバックと書き込み可能!
- Clone+F5で動作確認!すぐLチカとdefmtが始まります。

```sh
$ cargo build
$ elf2uf2-rs target/thumbv6m-none-eabi/debug/rp-rust-starter
```

