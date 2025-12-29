# Rust用Codespacesテンプレート

## オプション
```
$ sudo /root/.local/bin/mise i
```

## 初期設定
### プロジェクト作成
```
$ cargo new <プロジェクト名> --bin 
$ cd <プロジェクト名>
```

## 外部パッケージ（クレート）取得
Cargo.tomlの `[dependencies]` に利用するパッケージを追加
```
[dependencies]
rand = "0.8.5" # この行を追加
```
これで実行時に自動的に外部パッケージを取得してくれる。

## テスト実行
```
$ cargo test
```

## 実行
### コンパイルだけ行う
```
$ cargo build
```

### コンパイルして実行する
```
$ cargo run
```

## リリース用のモジュールを用意する
```
$ cargo build --release
```

## Ref
[とほほのRust入門](https://www.tohoho-web.com/ex/rust.html)  
[Rustでテストコードをどこに書くべきか](https://teratail.com/questions/208572)  
[Rustのテスト実行](https://ytyaru.hatenablog.com/entry/2020/09/17/000000)  
