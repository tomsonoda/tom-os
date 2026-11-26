# tom-os

LLMエージェントを動かすことだけに特化した、超軽量のベアメタルOSです。

アプリは最初から入っていません。起動するとエージェントのプロンプトが出て、「電卓を作って」のように頼むと、LLMがその場でアプリ（Lua）を書き、動作を確認し、そのままユーザーが使えます。

```
tom-os> 数当てゲームを作って
  [write_file] apps/guess.lua (612 bytes)
  [run_lua] 58 bytes ... ok
  [launch_app] apps/guess.lua
1から100の数を当ててください: _
```

Linuxなどの既存カーネルは使っていません。ブート、コンソール、メモリ管理、ネットワーク（virtio-net / ARP / IPv4 / TCP / HTTP）、JSON、エージェントのループまで、すべてこのリポジトリのコード（C + 起動部分のみNASM）です。既存コードを取り込んでいるのは、アプリの実行環境であるLua 5.4（`third_party/lua`、無改変）だけです。カーネル全体で約270 KiB、起動は一瞬です。

[llm.asm](https://github.com/alesha-pro/llm.asm) を参考にしていますが、方向が違います。llm.asm はカーネル内でモデルをローカル推論するチャットREPLで、tom-os はまず「ツールを使えるエージェント」と「その場で作ったアプリを動かす仕組み」を作っています。

## 現在の状態

最初のマイルストーンです。x86-64 の QEMU で動きます。

| できること | まだできないこと |
|---|---|
| QEMUで起動してエージェントと対話 | Android端末 / aarch64 |
| LLMがLuaアプリを生成・検証・起動 | OS内でのローカル推論（今はHTTP越しにLLMを呼ぶ） |
| シリアルとVGAテキストのコンソール | グラフィックUI、日本語のVGA表示 |
| ISOからの起動（BIOS） | UEFI起動、ディスクへのインストール |
| メモリ上のファイルシステム | 再起動後も残るストレージ |
| 平文HTTPでのLLM接続 | TLS、DHCP、DNS |

## 必要なもの

- `gcc`、`nasm`、`binutils`、`make`
- `qemu-system-x86_64`
- Python 3（テストとホスト側の中継プログラム用）

## ビルドとテスト

```sh
make          # build/tomos.elf を作る
make test     # QEMUで起動し、モックのLLMを相手に一連の動作を確認する
make iso      # build/tomos.iso を作る（GRUBでBIOS起動するCDイメージ）
make test-iso # 同じテストをISOからの起動で行う
```

ISOを作るには追加で `grub-pc-bin`、`xorriso`、`mtools` が必要です。ビルド済みのISOは、GitHubの Actions の実行結果（成果物 `tomos-iso`）からダウンロードできます。

`make test` は実際のLLMを使いません。`tools/mock_llm.py` が決め打ちの応答（ツール呼び出しを含む）を返し、起動 → HTTP → アプリ生成 → アプリの対話実行 → 電源断までを確認します。

## 動かす

tom-os はLLMを平文HTTPで呼びます。接続先はカーネル引数 `llm=IP:port` と `model=名前` で指定します（`make run` の `LLM=` と `MODEL=`）。QEMUのユーザーネットワークでは、ホストは `10.0.2.2` に見えます。

### Claude を使う

tom-os にはTLSがないので、ホスト側で中継プログラムを動かします。APIキーはホストに置いたままで、VMには入りません。

```sh
pip install anthropic
export ANTHROPIC_API_KEY=...          # または ant auth login
python3 tools/claude_gateway.py       # 127.0.0.1:8080 で待ち受け
```

別のターミナルで:

```sh
make run
```

モデルは既定で `claude-opus-5-5` です。`--model` と `--effort` で変えられます。

### ホスト上のローカルLLMを使う

llama.cpp の `llama-server` や Ollama のように、平文HTTPでchat completions形式（ツール呼び出し対応）を話すサーバーには直接つながります。

```sh
make run LLM=10.0.2.2:11434 MODEL=qwen3     # Ollama の例
```

### HTTPSが必要なその他のエンドポイント

```sh
export RELAY_API_KEY=...
python3 tools/http_relay.py --upstream https://example.com/v1/chat/completions
make run MODEL=<モデル名>
```

### 操作

`make run` はこのターミナルがシリアルコンソールになります。QEMUの終了は `Ctrl-A` のあと `X`、`Ctrl-C` は tom-os に届きます（実行中のアプリやLLM待ちを中断）。VGA画面も見たい場合は `make run-vga`。

日本語で話しかけられます（シリアルコンソール側）。VGAテキスト画面はASCIIしか表示できません。

| コマンド | 内容 |
|---|---|
| （普通の文章） | エージェントに頼む |
| `/apps` | ファイル一覧（アプリは `apps/<名前>.lua`） |
| `/run <名前>` | アプリを起動 |
| `/cat <パス>` `/rm <パス>` | ファイルの表示・削除 |
| `/lua <コード>` | Luaを1行実行 |
| `/clear` | 会話履歴を消す |
| `/info` | メモリ・ネットワーク・LLM設定 |
| `/set llm <IP:port>` `/set model <名前>` `/set path <パス>` | LLMの接続先とモデルを起動後に変える |
| `/halt` `/reboot` | 電源断・再起動 |

## Mac（macOS）で試す

x86-64 をエミュレーションするので、Apple Silicon でも Intel でも動きます。ISOは上記のとおり Actions からダウンロードできます（Mac上でのビルドは未対応）。

この節の手順は Linux 上の QEMU での確認に基づいています。macOS の QEMU と UTM では未確認です。

### QEMU（Homebrew）

```sh
brew install qemu
qemu-system-x86_64 -m 256M -cdrom tomos.iso -boot d \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 -serial mon:stdio
```

QEMUのウィンドウ（VGA画面）とこのターミナル（シリアル）の両方がコンソールになります。日本語を使うときはターミナル側から入力してください。

### UTM

UTM は QEMU に設定画面を付けた macOS 用の仮想マシンアプリです。新規作成で「エミュレート」→「その他」を選び、次のように設定します。

- アーキテクチャ: x86_64
- 起動: UEFIブートを無効にする（BIOS起動）
- メモリ: 256 MB 以上
- ネットワーク: 「Emulated VLAN」、カードは virtio-net-pci（既定の「Shared Network」ではつながりません）
- CD/DVD: `tomos.iso`

### LLMをMac上で動かす

VMからは Mac が `10.0.2.2` に見えます。GRUBのメニューで接続先（8080番 / Ollamaの11434番）を選べ、起動後は `/set` で変えられます。

```sh
# Ollama の場合（ツール呼び出しに対応したモデルが必要）
ollama serve
#   tom-os> /set llm 10.0.2.2:11434
#   tom-os> /set model <pull済みのモデル名>

# Claude の場合
pip install anthropic && export ANTHROPIC_API_KEY=...
python3 tools/claude_gateway.py
```

### 確認してほしい項目

1. ISOから起動してプロンプトが出る
2. キーボードで入力できる（US配列のみ対応。JIS配列では記号の位置がずれます）
3. `/info` で network が表示される
4. LLMと会話が1往復通る
5. 「〇〇アプリを作って」でアプリが生成され、`/run <名前>` で使える

## 仕組み

```
+---------------------------------------------------------------+
| shell (agent/shell.c)   プロンプト、/コマンド                   |
| agent (agent/agent.c)   会話履歴、LLM呼び出し、ツール実行        |
|   tools: write_file read_file list_files delete_file           |
|          run_lua (出力を捕まえて検証) launch_app (対話実行)      |
+----------------------------+----------------------------------+
| Lua 5.4 + lua/lbind.c      | net/http.c   HTTP/1.1 クライアント |
|   io / os / tomos / require| net/tcp.c    TCP (1接続, 再送あり) |
| fs/ramfs.c  メモリ内FS      | net/net.c    Ethernet, ARP, IPv4  |
+----------------------------+----------------------------------+
| kernel/  console (シリアル+VGA+PS/2)  mem (ヒープ)  cpu (IDT,    |
|          PIC, タイマ, RTC)  virtio_net (PCI, レガシーvirtio)     |
| lib/     最小libc, printf(浮動小数点対応), libm(x87), JSON       |
| arch/x86_64/boot.asm  Multiboot1 → ロングモード                 |
+---------------------------------------------------------------+
```

- 単一アドレス空間・単一タスクです。デバイスはすべてポーリングで、割り込みは `HLT` からCPUを起こすためだけに使います（待機中にCPUを使い切りません）。
- アプリは実行のたびに新しいLuaステートで動きます。メモリ上限があり、`Ctrl-C` と時間制限で必ず止められます。
- LLMとのやりとりはchat completions形式（`tools` 付き）のJSONです。応答は `Content-Length` と chunked の両方に対応しています。

### アプリから使えるAPI

Lua標準の `string` `table` `math` `utf8` `coroutine` に加えて:

- `io.write` `io.read` `io.lines` `io.open`（メモリ内FSに対して読み書き）
- `os.time` `os.date` `os.clock` `os.remove` `os.rename` `os.exit`
- `tomos.cls()` `tomos.getkey([timeout_ms])` `tomos.sleep(ms)` `tomos.readline([prompt])` `tomos.time_ms()` `tomos.files()` `tomos.read_file(path)` `tomos.write_file(path, data)`
- `require('name')`（`name.lua`、`apps/name.lua`、`lib/name.lua` を探す）
- 画面制御はANSIエスケープシーケンス（色、カーソル移動、消去）

## 制限と注意

- 生成されたアプリはカーネルと同じ権限で動きます。Luaの中に閉じ込めてはいますが、プロセス分離やメモリ保護はありません。
- ファイルはメモリ上にあり、再起動で消えます。
- IPアドレスは固定です（既定 `10.0.2.15`、ゲートウェイ `10.0.2.2`。カーネル引数 `ip=` `gw=` `mask=` で変更可）。
- 会話履歴は切り詰めません。長くなったら `/clear` してください。
- 実機（PC）での起動は未確認です。virtio-net 以外のNICドライバはありません。

## 次の段階の候補

1. 永続ストレージ（virtio-blk）で、作ったアプリを残す
2. LLM応答のストリーミング表示
3. カーネル内でのローカル推論（小型モデル）
4. aarch64 対応（Android端末に向けて）
5. フレームバッファによるグラフィックUIと日本語表示
6. アプリの隔離（ユーザーモード、メモリ保護）

## ライセンス

`third_party/lua` は Lua.org, PUC-Rio のMITライセンスです（`third_party/lua/README.md`）。
