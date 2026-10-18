# Myrica EME (Encrypted Media Extensions) 実装ロードマップ

調査日: 2026-10-16

対象リポジトリとピン:
- myrica (ブランチ feature/webkit-backend)
- WebKit fork petitstrawberry/WebKit の scarlet ブランチ (platforms/scarlet/CMakeLists.txt のピン: 634e4abf34c73cf37a692251acc284ed57f4b919)
- Scarlet (user/lib/scarlet-codecs / petitstrawberry/Symphonia fork)

## 結論

- EME 自体は実装可能。WebCore の EME V3 一式 (NavigatorEME, MediaKeySystemAccess, MediaKeys, MediaKeySession, MediaKeyStatusMap, CDMFactory, CDMInstance, CDMInstanceSession, CDMProxy, InitDataRegistry, ClearKey CDM) は WebKit fork に上流のまま入っており、Scarlet ポートのビルド設定で無効化されているだけ。ビルドフラグを立て、ポート側の CDM 登録とプレイヤフック、および Rust 側の AES-128-CTR 復号を追加すれば成立する。
- Widevine も技術的には射程に入る。ARM64 Linux 向け CDM バイナリ (WidevineCdm/_platform_specific/linux_arm64/libwidevinecdm.so) は存在し、ChromeOS イメージからの抽出が ARM64 の実績ある入手方法 (Asahi Linux の widevine-installer が同一手法)。ただし CDM はプロプライエタリで、Google の個別ライセンスなしの再配布・同梱は不可 (Chromium の third_party/widevine/LICENSE)。配布物には含めず、ユーザーが自分で用意する運用が前提になる。L3 のみで、L1/L2 と HDCP 出力制限は対象外。
- EME は WebKit バックエンド限定 (backend-webkit-vulkan + --media のプロファイル)。Blitz バックエンドは再生パイプライン自体を持たないため対象外。
- 商用サイトの再生には Widevine が必要で、これを Phase 4 の Widevine トラックで扱う。CDM はユーザー各自が用意し、Linux ビューのヘルパープロセスでホストする構成にする。

## 現状調査

myrica 側:
- src/ と crates/myrica-webkit-sys/ に EME/DRM 関連コードは無い (rg で確認)。
- 再生は ScarletMedia C ABI (fork 側 ScarletMedia.h) → Rust media_bridge.rs → scarlet-codecs (Symphonia fork の IsoMp4Reader + HW/SW decode) の経路。暗号化サンプルという概念がまだ無い。

WebKit fork 側 (上流のまま存在、無効化されているもの):
- Source/cmake/WebKitFeatures.cmake: ENABLE_ENCRYPTED_MEDIA (235 行目), ENABLE_LEGACY_ENCRYPTED_MEDIA (254 行目), ENABLE_THUNDER (306 行目, ENABLE_ENCRYPTED_MEDIA 依存)。
- Source/cmake/OptionsScarlet.cmake: ポート機能リストで ENABLE_ENCRYPTED_MEDIA / ENABLE_LEGACY_ENCRYPTED_MEDIA を OFF に設定。
- Source/WebCore/Modules/encryptedmedia/: JS API 一式。InitDataRegistry.cpp は keyids / cenc / webm の init data を登録済み。
- Source/WebCore/platform/encryptedmedia/: CDMFactory, CDMInstance, CDMInstanceSession, CDMProxy, CDMInstanceProxy, CDMInstanceSessionProxy, CDMProxyFactory, clearkey/CDMClearKey.{h,cpp} (CDMFactoryClearKey, CDMPrivateClearKey, CDMInstanceClearKey, CDMInstanceSessionClearKey)。
- CDMFactory.cpp には非 GStreamer / 非 AVFoundation ポート向けの platformRegisterFactories スタブがある。CDMProxy は「decryptor がバックグラウンドスレッドから使う」スレッド安全 API と明記されている。
- 参考実装: Source/WebCore/platform/graphics/gstreamer/eme/。CDMFactoryGStreamer.cpp は Thunder のみ登録し、CDMProxyThunder.h は decode スレッド向けの thread-safe API の実例になっている。
- Scarlet ポート側: MediaPlayerPrivateScarlet / MediaSourcePrivateScarlet / SourceBufferPrivateScarlet / MediaSampleScarlet / ScarletMedia.h に EME 関連実装は無い。

EME を有効化したときに Scarlet ポートが実装すべきフック (上流インタフェース):
- MediaPlayerPrivate.h: cdmInstanceAttached / cdmInstanceDetached / attemptToDecryptWithInstance / waitingForKey。
- MediaPlayer.h: mediaPlayerInitializationDataEncountered / mediaPlayerWaitingForKeyChanged / mediaPlayerMediaKeysStorageDirectory。
- CDMPrivateClearKey は robustness が空文字列のみ対応 (上流の制約)。

Scarlet OS 側:
- user/lib/sunset が aes 0.8 + ctr 0.9 + cipher 0.4 を Scarlet 向けに既にビルドしている (Aes256Ctr32BE を使用中)。AES-128-CTR (Ctr128BE<Aes128>) は同じクレートで扱える。
- user/lib/scarlet-codecs/src/media.rs (1109 行) の Mp4Parser は Symphonia fork を包んでおり、senc / saiz / saio / tenc / sinf / pssh の解析は無い。
- tests/webkit_server.py の --media-fixtures は ffmpeg で H.264/AAC を生成する。ローカルの ffmpeg 8.1.2 は mp4 muxer の -encryption_scheme cenc-aes-ctr / -encryption_key / -encryption_kid に対応済み。

## 設計方針

1. ビルドフラグ: fork で ENABLE_ENCRYPTED_MEDIA=ON (LEGACY は OFF のまま)。myrica では platforms/scarlet/build-webkit.py に --eme を追加し、myrica-webkit-sys の Cargo feature から --media と一緒に渡す。デスクトップ既定の Blitz には影響させない。
2. CDM 登録 (fork): Scarlet ポート用の CDMFactory / CDMProxyFactory を追加して ClearKey (org.w3.clearkey) を登録し、CDMFactory.cpp / CDMProxy.cpp の非 GStreamer スタブを Scarlet では外す。CDMInstanceClearKey は CDMInstanceProxy 経由で CDMProxy に鍵を merge するため、CDMProxyFactory 側の登録が必須。
3. プレイヤ (fork): MediaPlayerPrivateScarlet にフックを実装。pssh などの init data は InitDataRegistry を使い "cenc" / "keyids" として initializationDataEncountered に通知する (プログレッシブは受信バイト列、MSE は SourceBufferPrivateScarlet の append バイト列から抽出)。
4. 復号の場所: 既存方針 (Page スレッドを塞がない、bounded worker 境界) に合わせて Rust の decode worker (media_bridge.rs) で復号する。AES-128-CTR のサブサンプル復号は scarlet-codecs 側に実装。
5. 鍵配送: fork 側に ClearKey 用 CDMProxy サブクラス (CDMProxyThunder と同型の thread-safe API) を用意し、Rust worker から C ABI のコールバックで鍵 ID を問い合わせる。C++ 側は CDMProxy のスレッド安全な鍵取得 (getOrWaitForKeyValue 相当 + CDMProxyDecryptionClient) を呼ぶ。ライセンス更新や鍵ローテーションは CDMInstanceSessionClearKey → CDMProxy の KeyStore 更新として反映される。
6. ABI 拡張 (fork + myrica): ScarletMedia.h / media_bridge.rs に (a) サンプルの暗号化メタデータ (encrypted フラグ, key ID, IV, サブサンプル範囲, scheme), (b) 鍵解決コールバック, (c) status の waiting_for_key を追加。既存の上限 (8 MiB / 512 パケット / 3 フレーム) は維持する。

## ロードマップ

### Phase 0: ビルドスパイク (規模: 小)
目的: ENABLE_ENCRYPTED_MEDIA=ON の Scarlet ビルドが通ること、既存機能に回帰がないこと、JS API の存在を確認する。
作業:
- fork を -DENABLE_ENCRYPTED_MEDIA=ON で検証ビルド (コミット前)。
- コンパイルエラーの棚卸し (非 GStreamer ポートで不足するコードパス、Settings や MediaKeySystemController 周り)。
- webkit-smoke の通常チェックと --media-only で回帰確認。
- requestMediaKeySystemAccess が CDM 未登録のため NotSupportedError を返すことを確認。
Exit 条件: flag ON のビルドで既存チェックが全て通る。EME 有効化のコストとリスクが確定している。

### Phase 1: ClearKey + プログレッシブ MP4 (規模: 中〜大)
fork:
- CDMFactoryScarlet.cpp (仮称) と ClearKey 用 CDMProxy サブクラス、CDMFactory.cpp / CDMProxy.cpp のスタブ修正。
- MediaPlayerPrivateScarlet: CDM フック 4 種、waitingForKey、init data 抽出と initializationDataEncountered、mediaKeysStorageDirectory。
myrica:
- ScarletMedia.h / media_bridge.rs / lib.rs の ABI 拡張。
- 復号・鍵解決・waitingForKey の実装、build-webkit.py --eme、Cargo feature、README 更新。
Scarlet:
- scarlet-codecs: tenc / senc / saiz / saio / sinf / pssh の解析 (Symphonia fork に手を入れるか Scarlet 側の pre-pass にするかは本フェーズ先頭で決定) と AES-128-CTR サブサンプル復号。
テスト:
- tests/webkit_server.py --media-fixtures に CENC フィクスチャ (ffmpeg -encryption_scheme cenc-aes-ctr、固定キー/KID) と ClearKey ライセンス JSON エンドポイントを追加。
- webkit-smoke に EME チェックを追加 (requestMediaKeySystemAccess → MediaKeys → session → generateRequest → update → 再生 → keyStatuses=usable → 画素/音声検証、ライセンス遅延時の waiting と encrypted イベント検証)。
Exit 条件: 暗号化 MP4 (プログレッシブ) が ClearKey で再生され、画素と音声、鍵ステータスが QEMU 上で検証できる。鍵なし/不正鍵の失敗経路が明示的に失敗する。

### Phase 2: MSE / フラグメント (規模: 中)
- 暗号化 init セグメント (pssh / tenc) と暗号化メディアセグメント、複数 KID (鍵ローテーション)、遅延ライセンス。
- SourceBufferPrivateScarlet / MediaSourcePrivateScarlet からの init data と暗号化サンプルの伝搬。
Exit 条件: 暗号化 fMP4 の MSE 再生が --media-only 拡張フィクスチャで検証できる。既存の非暗号化 MSE に回帰がない。

### Phase 3: 仕上げとサイト検証 (規模: 小〜中)
- persistent-license セッションと mediaKeysStorageDirectory (HOME/.local/share/myrica 系)、session close/remove、エラー処理 (ライセンス拒否、鍵リボーク)。
- keyids init data type、encryptionScheme (cbcs = AES-CBC パターン暗号) の要否判断と対応。
- 実サイト検証: ClearKey デモ (Shaka Player / dash.js の ClearKey ストリーム等)。復号コストと Page スレッドへの影響の計測。
- README / ドキュメント更新 (対応範囲と非対応範囲の明記)。

### Phase 4: Widevine トラック (規模: 大。EME 基盤 (Phase 1〜3) と並行可能)

前提: CDM バイナリはプロプライエタリで、Google との個別契約なしに同梱・再配布できない (Chromium third_party/widevine/LICENSE、パートナー登録が必要な Widevine アクセスページ)。配布形態は「ユーザーが自分の CDM を用意して所定の場所に置く」方式に限定する。ARM64 は ChromeOS/LaCrOS イメージからの抽出が実績ある入手方法で、Asahi Linux の widevine-installer が同じ手法をとっている。Chrome for ARM64 Linux は 2026-03-12 に発表され、Q2 2026 の提供開始が予定されている。

入手: Asahi Linux の widevine-installer が同じ抽出を自動化する。Debian では trixie-backports (contrib) の widevine-installer パッケージが利用でき、apt で導入した後に sudo widevine-installer を実行する (パッケージ導入だけでは完了しない)。aarch64 専用、glibc 2.36 以上、root 実行が必要。配置先は /var/lib/widevine/WidevineCdm/<version>/_platform_specific/linux_arm64/libwidevinecdm.so で、Chromium 用のシンボリックリンクも作られる。Scarlet では Debian ビュー内での実行が想定経路になる (要検証: apt とネットワークが Linux ABI で動くこと)。

構成 (Scarlet の既存機構に乗せる):
- CDM は Linux/glibc バイナリなので Scarlet ネイティブの myrica から直接 dlopen できない。Linux ビュー (Debian glibc rootfs) で動くヘルパープロセスに CDM をホストさせ、myrica とは共有メモリ + ソケットの IPC で接続する。起動は Scarlet の Environment / ABI view の spawn API (scarlet_os::environment、abi-run) を使う。
- ヘルパー内の CDM ホストは 2 案。(a) RDKCentral の OCDM-Widevine / widevine-rdk (OpenCDM + adapter。CDM 本体は別途用意) を利用する。(b) Chromium の content_decryption_module.h (オープン) 向けに自前のホストを書く。WebKit fork には ENABLE_THUNDER の OpenCDM 統合 (CDMThunder / CDMProxyThunder) が既にあるが、CDMThunder は OpenCDM のプロセス内 API 前提で、CDMProxyThunder の復号 API は GstBuffer / GstCaps 依存のため、Scarlet では (b) 相当の自前ブリッジが必要になる。
- WebKit 側は Widevine (com.widevine.alpha) の CDMPrivate / CDMInstance を IPC 経由で実装する。init data (pssh、Widevine SystemID) の伝搬、MediaKeys / セッション、ライセンスメッセージは ClearKey 基盤と共通。
- メディア経路: 暗号化サンプル (圧縮のまま、数百 KB) を共有メモリでヘルパーへ渡し、CDM に復号させる。映像は CDM のビデオデコーダコールバック (DecryptAndDecode 相当) と Scarlet のデコーダの接続が必要で、ここは要検証。

W0 実施記録 (2026-10-16):
- CDM 抽出: ChromeOS Strongbad (SC7180, arm64) リカバリイメージの ROOT-A (ext4) から debugfs で libwidevinecdm.so を抽出した。バージョン 4.10.2662.3、CDM interface / host version 10、コーデック avc1/vp09/av01、暗号化スキーム cenc/cbcs。エクスポートは CreateCdmInstance と GetCdmVersion。
- 依存: NEEDED は libdl.so.2 / libpthread.so.0 / libnspr4.so / libm.so.6 / libc.so.6 / ld-linux-aarch64.so.1。glibc シンボルは 2.29 までで Trixie (2.41) には十分。Debian ビューには libnspr4 が必要。
- ブロッカーと対処: 未修正の CDM は glibc 2.36 以降で DT_RELR without GLIBC_ABI_DT_RELR dependency により dlopen に失敗し、さらに libgcc が公開しない __aarch64_ldadd4_acq_rel / __aarch64_swp4_acq_rel を参照する。Asahi Linux の widevine_fixup.py (MIT) が両方を注入する。4 KiB ページでは RELRO / W^X を壊さない (Scarlet の aarch64 MMU もコンテナも 4 KiB)。fixup 後の修正版も再配布は不可。
- プローブ実装: myrica の tools/widevine-probe に Chromium CDM ABI の Host_10 実装を追加した (dlopen → InitializeCdmModule_4 → CreateCdmInstance → Initialize → CreateSessionAndGenerateRequest → メッセージ取得、鍵ステータス表示、更新応答の投入、ダンプ、タイムアウト、終了コード)。
- ドライラン結果: 修正済み CDM に対し Debian Trixie (glibc 2.41) aarch64 の Docker で、dlopen → initialize ok → license-request 1676 bytes の生成まで成功 (PROBE_RESULT ok)。ライセンス要求には platform_name ChromeOS、model_name ChromeCDM、widevine_cdm_version 4.10.2662.3 などが含まれる。
- Scarlet 実機での実測 (2026-10-17 完了): full プロジェクト (projects/aarch64-limine-full, SCARLET_QEMU_DISPLAY=none) の linux-aarch64 ビューで、ホストの HTTP サーバ (10.0.2.2:8777) からプローブと修正済み CDM を /shared に取得して abi-run で実行した。dlopen → InitializeCdmModule_4 → CreateCdmInstance (GetCdmHostFunc version=10) → Initialize success=true → セッション作成 → license-request 1677 bytes の生成まで成功し、終了コード 0。uname は Linux 6.1.0-scarlet_linux_abi_module、AT_HWCAP=0x2007fb (AES/PMULL/SHA 系を含む)、AT_HWCAP2=0x0。CDM は Scarlet の Linux ABI 上で初期化とライセンス要求生成まで動作した。
- 実測手順の注意: Debian フルのイメージには .shrc が無く LD_LIBRARY_PATH は未設定 (echo など単純なバイナリは動作)。libnspr4 はゲストの apt で導入した (ゲストの外向きネットワークは slirp 経由で動作)。シリアルコンソールのシェルは長いコマンド行が途中で切れるため、ホストから短い名前で配るスクリプトを curl -so /shared/w http://10.0.2.2:8777/w → sh /shared/w の形で実行するのが確実。ゲストからホストへは 10.0.2.2 で到達可能。

### W1 設計: Widevine ホストプロセス + IPC / 共有メモリ

- プロセス構成: myrica (scarlet ABI のネイティブプロセス) が widevine-host (linux-aarch64 ビュー、glibc) を Environment の spawn で起動し、CDM はそのプロセス内でのみ dlopen する。ネイティブ側は glibc のコードを一切ロードしない。Chromium の out-of-process CDM と同じ形で、クラッシュ隔離と「CDM が動ける唯一の場所で動かす」ことを同時に満たす。
- 起動とハンドル: spawn の handle map で制御ソケット (socketpair の片端) と共有メモリハンドルを明示的に渡す。stdio は継承されないため、必要なものは必ずマップで渡す。PDFview が zathura にファイルを渡しているのと同じ機構。
- 制御プロトコル: 長さ前置の TLV。Initialize / CreateSession (temporary, persistent) / Update (license または provisioning 応答) / Close / Remove / Message (license-request, individualization-request) / Keys (key id と status) / Decrypt / Status / Shutdown。CDM 呼び出しはホスト内の 1 本のワーカースレッドに直列化する (CDM は単一スレッド前提)。
- データ経路: 共有メモリのスロットリング。ヘッダに {slot, offset, size, generation, lease} を持ち、暗号化サンプル (圧縮のまま数百 KB) を書いて Decrypt 要求に {slot, key_id, iv, subsample map} を載せる。CDM の応答 (平文圧縮サンプル) は別スロットに書いて generation で再利用を管理する。フレームの lease は既存の frame_lease と同じ考え方で、UI が消費し終わるまで返却しない。
- CDM API の選択: 映像は DecryptAndDecode ではなく Decrypt を使い、復号済みの圧縮サンプルを Scarlet の既存 H.264 パイプライン (scarlet-codecs / video client) に流す。CDM が Decrypt を拒否する場合は VideoDecoder / VideoFrame コールバックの実装が必要になるので、この可否は W1 のスパイクで先に確定させる。
- WebKit 側: com.widevine.alpha の CDMPrivate / CDMInstance / CDMInstanceSession を IPC クライアントとして実装する。MediaKeys の非同期通知 (session message, keys change, expiration) はプロトコルの通知とそのまま対応する。
- プローブとの関係: tools/widevine-probe の Host_10 実装がそのままホストの心臓部になる。W1 はイベントループ、プロトコル、共有メモリを足すだけで、捨てる作業はない。
- 共有メモリの受け渡し (2026-10-17 ソース確認済み): ネイティブ側で sys_shared_memory_create → scarlet_os::ipc::SharedMemory::create(size, permissions) により確保し、Environment::spawn の HandleMapping { source, target } で子に渡すと、子のハンドル番号は target になる。Linux イメージの実行時は LinuxAbi::prepare_exec_fds が image のハンドルをそのまま fd に写像する (fd_to_handle[handle] = handle、handle < MAX_FDS = 1024、CloseOnExec メタデータは FD_CLOEXEC に反映)。
- Linux の mmap は fd → handle → カーネルオブジェクトを引き、MemoryMappingOps と supports_mmap() を要求する。SharedMemory は supports_mmap() = state.valid、MAP_SHARED では paddr = base + offset と is_shared = true を返し、get_mapping_info_with は既定実装で get_mapping_info に委譲するため、ネイティブ側の mmap (Syscall::MemoryMap) と同じ物理ページを共有する。on_mapped / on_unmapped と mapping_count で寿命も管理される。
- 制御チャネルはネイティブの socketpair (syscall 905) または pipe を同じ HandleMapping で渡せる。したがって「不可ならソケットコピーで開始」というフォールバックは不要で、最初から共有メモリと fd 渡しで設計できる。target は 1024 未満かつ未使用の番号 (3, 4, ...) を選ぶ。
- 障害時: ホストの異常終了を myrica が検知したらセッションを失効させる (再生はエラーで終了)。CDM の状態はホスト内に閉じているので、ホストを起動し直せば作り直せる。
- installer の位置付け: 調達と fixup の自動化であり、アーキテクチャの一部ではない。ホストプロセスが読む .so を正しい場所 (/var/lib/widevine/WidevineCdm/<version>/_platform_specific/linux_arm64/libwidevinecdm.so) に用意する役割で、プローブはこのパスを既定で探す。
- W1 実装 (2026-10-17, 最初のスライス完了): tools/widevine-host に CDM ホストプロセスを追加した。16 バイトヘッダの制御プロトコル (HELLO / INIT / CREATE_SESSION / UPDATE_SESSION / CLOSE_SESSION / REMOVE_SESSION / DECRYPT / RELEASE / SHUTDOWN と READY / RESULT / SESSION / MESSAGE / KEYS / EXPIRATION / SESSION_CLOSED)、4096 バイトヘッダ + 4 x 1 MiB スロットの共有領域、スロット経由のライセンス要求配送を aarch64 Debian コンテナで実測した (READY → INIT status=0 → EV_SESSION → EV_MESSAGE slot=0 size=1676 → CLIENT_OK)。CDM 呼び出しはホスト内の要求ループで直列化し、イベントは専用スレッドからソケットへ書く。イベントは非同期なので、クライアントは固定の応答順ではなくイベント種別で処理する必要がある (実測で EV_SESSION / EV_MESSAGE が RESULT より先に届く)。
- Scarlet 実機での W1 実測 (2026-10-18 完了): ネイティブの tools/widevine-host-client が SharedMemory (4096 + 4 x 1 MiB) と socketpair を作り、spawn_with_abi の handle map で stdio を 0/1/2、socket を 3、共有メモリを 4 に渡して widevine-host を起動した。ホストは Linux ビューで CDM 4.10.2662.3 をロードして初期化し、セッションを作成、license-request 1677 bytes をスロット 0 に書いた。クライアントはマップ済み領域から先頭バイト (8,1,18,239) を読み、WVHC_OK で終了。ホストは status=0 で終了した。
- 実測で判明した Linux ABI の注意: SharedMemory の fd に対する fstat は失敗するため、ホストは呼び出し側が渡すレイアウトを正とする。spawn では標準入出力も明示的に map しないと子の出力が届かない (handle map に 0/1/2 を入れる)。
- ライセンス往復の実測 (2026-10-18 完了): Shaka の Sintel Widevine アセット (storage.googleapis.com/shaka-demo-assets/sintel-widevine/dash.mpd) から取り出した 167 バイトの PSSH で、CDM は 1797 バイトのライセンス要求を生成した。コンテナのクライアントは TLS を持たないため、ホスト側の license-relay.py が HTTPS 終端として cwip-shaka-proxy.appspot.com/no_auth に転送し、1229 バイトの応答を返した。UPDATE_SESSION は status=0、CDM は 5 つの鍵を usable (status=0) として報告した。実際のブラウザ統合ではページの fetch() がライセンス POST を行うため、ブラウザ側にライセンス用のネットワーク実装は不要になる。
- 実データでの復号 (2026-10-18 完了): Axinom の v7-MultiDRM-SingleKey テストベクタ (dash.js のサンプルが公開している X-AxDRM-Message トークンを使用) で、テンポラリセッションに鍵が 1 つ usable として入り、音声サンプル (341 バイト、サブサンプルなし) が kSuccess で復号された。映像サンプルでは、単一の暗号化領域を指定したときに先頭 NAL 長 (755 バイト) がバイト一致で復元された。誤った鍵 ID では kNoKey が返るため、成功は鍵に基づいている。
- W1 の初期診断の訂正 (2026-10-18): サブサンプルを同じ IV で個別に復号する方式を採用し、96 サンプル中 16 件の NoKey / ゼロ出力を CDM の不安定性と判断していた。この判断と「映像は DecryptAndDecode が必須」という結論を撤回する。Chromium CDM API は全暗号化領域を連結した一つのストリームとして扱い、クリア領域を復号入力から除外する契約である。単一フレームの参照表示だけでは、旧方式の正当性は確認できていなかった。
- 現在の復号方式: 各暗号化領域を順に連結し、単一の `{clear=0, cipher=合計}` で CDM の Decrypt を一度呼ぶ。結果を元のサブサンプル位置へ戻し、クリア部分を保存する。IV はサンプルにつき一度使う。ゼロ出力の再試行は削除した。同じ CDM 4.10.2662.3 で、従来停止した Sintel の区間を Scarlet 実機で連続デコードできた。別ビルドの CDM への交換は不要だった。
- DecryptAndDecodeFrame の単体試験は過去の診断であり、現在の映像経路には使わない。CDM は圧縮サンプルの復号、Scarlet `/dev/video0` が映像デコードを担当する。
- コンテナ解析: Symphonia fork が `sinf` / `tenc` とフラグメント内 `senc` を読み、KID・IV・サブサンプル対を packet に対応させる。`stsd` のクリア用/暗号化用エントリはフラグメントの sample description index で選ぶ。`moov` 内の PSSH は一つの cenc init data にまとめる。
- W1/W2 の橋渡し (2026-10-18): `crates/cdm-host` は共有メモリと socketpair、linux-aarch64 ホスト起動 (stdio 0..2、socket 3、共有メモリ 4)、初期化・セッション・復号・終了を提供する。制御応答の途中で届く非同期イベントを保持し、部分的なソケットフレームも次のポーリングへ引き継ぐ。
- W2 の接続方針: WebKit 側の CDMPrivate / CDMInstance / CDMInstanceSession は、ScarletWebView C ABI に追加する関数 (create / destroy / initialize / create_session / update_session / close_session / decrypt / take_event) 経由でこのライブラリを呼ぶ。init data は InitDataRegistry、ライセンス POST はページの fetch() が担当するため、ブラウザ側にライセンス用ネットワークは不要。

W0 (規模: 小) Linux ビューでの実証。Debian glibc ビューで libwidevinecdm.so を dlopen し、CDM の初期化とライセンス往復 (テスト用ライセンスサーバ) の可否を確認する。Linux ABI のギャップ (epoll スタブ、fcntl の F_DUPFD 欠落、シグナル簡易実装、auxv/TLS 整合) をここで洗い出す。最大の未知はここ。
W1 (規模: 中) ヘルパープロセスと CDM ホストの実装。license request / response と鍵取得まで通す。
W2 (規模: 中) WebKit 側の com.widevine.alpha 統合 (CDMPrivate / CDMInstance / IPC、init data、鍵ステータス)。
W3 (規模: 大) メディア復号統合 (共有メモリ経路、鍵ローテーション、シーク、映像デコーダ接続) と QEMU での画素検証。
W4 (規模: 中) 運用 (CDM の配置・更新、エラー処理、性能、ライセンス表記)。L3 のみで、platform verification (VMP 相当) を満たせず拒否されるサービスがありうる。

ClearKey との関係: Phase 1〜3 の EME 基盤 (init data、MediaKeys、MSE の暗号化セグメント、鍵ステータス) は Widevine でもそのまま必要になる。差し替わるのは復号エンジンだけなので、先に ClearKey で基盤を通すのは回り道にならない。

参考 (外部):
- Chrome for ARM64 Linux 発表: https://blog.google/chromium/bringing-chrome-to-arm64-linux-devices/
- Chromium の Linux arm64 Widevine 対応: https://chromium.googlesource.com/chromium/src/+/master/third_party/widevine/cdm/widevine.gni
- ARM64 CDM の入手 (Asahi Linux): https://github.com/AsahiLinux/widevine-installer
- 再配布不可のライセンス: https://chromium.googlesource.com/chromium/src/third_party/+/master/widevine/LICENSE と https://developers.google.com/widevine/access
- OpenCDM 系: https://github.com/rdkcentral/OCDM-Widevine と https://github.com/rdkcentral/widevine-rdk

## リスクと未確定点

1. ENABLE_ENCRYPTED_MEDIA の Scarlet ビルド可否は未検証 (Phase 0 で確定)。GStreamer / AVFoundation 以外のポートで不足する上流コードがある可能性。
2. ClearKey は WebKit 上では CDMFactory (JS 側) と CDMProxy (復号側) の両輪。上流は ClearKey 用 CDMProxy を提供しないため、ポート側で用意する必要がある。
3. 暗号化メタデータの解析を Symphonia fork に足すか Scarlet の Mp4Parser に足すかで、変更するリポジトリと規模が変わる。
4. waitingForKey の状態を Rust worker → C++ → WebCore へ正しく伝搬しないと、再生が固まるか早すぎる失敗になる。
5. フィクスチャ生成は ffmpeg の CENC 対応 (fMP4 / MSE 分割) に依存する。
6. セキュリティ: 復号はソフトウェアで、平文がメモリに載る。L1 相当の保護パスは提供できない。CDM バイナリを扱う場合のサンドボックス設計は将来課題。
7. ライセンス: Widevine CDM はプロプライエタリで、個別契約のない再配布・同梱は不可。配布物に含めず、ユーザーが用意する運用が必須。 fixup 後の修正版も同様に再配布不可。
8. Widevine CDM は Linux/glibc バイナリのため、Scarlet の Linux ABI のギャップ (epoll スタブ、fcntl の F_DUPFD 欠落、シグナル簡易実装、auxv/TLS 整合) がそのまま障害になりうる。W0 の実測で確定させる。
9. CDM ホストの platform verification (VMP 相当) を満たせないため、一部サービスが L3 でも拒否する可能性がある。L1/L2 と HDCP 出力制限は対象外。

## 参考: 主要ファイル

WebKit fork (scarlet ブランチ, rev 634e4abf34c73cf37a692251acc284ed57f4b919):
- Source/cmake/OptionsScarlet.cmake
- Source/cmake/WebKitFeatures.cmake
- Source/WebCore/Modules/encryptedmedia/ (InitDataRegistry.cpp ほか)
- Source/WebCore/platform/encryptedmedia/ (CDMFactory.cpp, CDMProxy.cpp, clearkey/CDMClearKey.h)
- Source/WebCore/platform/graphics/scarlet/ (MediaPlayerPrivateScarlet.h, ScarletMedia.h, MediaSampleScarlet.h, MediaSourcePrivateScarlet.h, SourceBufferPrivateScarlet.h)
- Source/WebCore/platform/graphics/gstreamer/eme/CDMProxyThunder.h (設計の参照)

myrica:
- platforms/scarlet/build-webkit.py, platforms/scarlet/CMakeLists.txt (fork ピン)
- crates/myrica-webkit-sys/src/media_bridge.rs, crates/myrica-webkit-sys/src/lib.rs, crates/myrica-webkit-sys/Cargo.toml
- tests/webkit_server.py, examples/webkit_smoke.rs, Cargo.toml

Scarlet:
- user/lib/scarlet-codecs/src/media.rs, user/lib/scarlet-codecs/Cargo.toml
- user/lib/sunset/Cargo.toml (aes / ctr の実績)

## W2 統合作業 (2026-10-18)

`feature/widevine-eme` の W0/W1 と Rust ブリッジを
`feature/webkit-backend` へマージした。WebKit fork の未公開コミット
`6a69964b` は `scarlet` に push し、その上に接続修正を積んだ。

- `ENABLE_ENCRYPTED_MEDIA` を明示的に切り替え可能にし、Myrica の
  `encrypted-media` feature → `build-webkit.py --eme` を接続。
- `CDMFactory` / `CDMProxyFactory` の登録、`ImplementationType::Scarlet`、
  Page の `MediaKeySystemClient` を追加。大文字のキーシステム名、
  ハードウェア保護、永続セッション、cbcs の要求は対応対象から外す。
- CDM の spawn / initialize / session / update / close は WebKit の
  serial `WorkQueue` で実行し、EME completion を元の Page owner の
  `RunLoop` へ戻す。ライセンス HTTP はページ自身の `fetch()` が担当する。
- C++ の関数名を Rust が実際に export する `cdm_host_*` に統一。
  `create_session` は初回メッセージ種別を返し、同じメッセージを
  イベントキューに重ねて入れない。CDM ABI の persistent-license は 1。
- ネイティブクライアントは要求メッセージをコピーした後に共有スロットを
  `RELEASE`。ホストは session id と初回 message の両方が配送キューに
  入ってから成功応答する。共有範囲・復号出力サイズを検証し、spawn 失敗や
  client drop でもマッピングを解放する。
- 既存 CLI が IPC 実装を重複して持っていたため `cdm-host` を使用する形に
  揃えた。`--sessions N` でスロット数を超える連続作成・close を検証する。
  間違って追跡されていた Cargo target / Python bytecode は管理対象から除外。

W2 の実機テストは `tests/eme-widevine.html` と既存の
`webkit-smoke URL --eme-only` を使う。ユーザーが用意した CDM とホストを
所定のパスへ置き、必要なら `MYRICA_WIDEVINE_HOST` /
`MYRICA_WIDEVINE_CDM` / `MYRICA_WIDEVINE_LD_PATH` を設定する。
この fixture は secure context が必要。配信はホスト側の既存サーバーを使う:

```sh
# ホスト
python3 tests/webkit_server.py --port 18769
# QEMU ゲスト。HTTP の localhost をホストの fixture サーバーへルーティング。
# HTTPS の MPD / ライセンス通信は通常の経路を使う。
export http_proxy=http://10.0.2.2:18769
webkit-smoke http://localhost:18769/eme-widevine.html --eme-only
unset http_proxy
```

検証用 HTTP proxy はゲストの環境変数だけで指定し、製品コードの
TLS 検証・secure context 判定には変更を加えない。サーバーは localhost に
bind し、固定 fixture のみを返す。CDM バイナリは配信しない。
Shaka Sintel の公開 MPD/PSSH とライセンスサービスを用い、拒否すべき設定、
6 セッションの初回メッセージ 1 回・ライセンス更新・usable キー・close を検査する。
CDM バイナリやライセンス応答をリポジトリ・配布物には含めない。

ここまでの検証: Scarlet 向け C++ syntax check、`cdm-host` の
`aarch64-unknown-scarlet` check、Linux aarch64 のホストビルドと実 CDM の
初期要求配送、Myrica の回帰テスト 77 件が成功。
EME 有効の全体 Release ビルドも成功。
メディアの encrypted イベント / CENC メタデータ / 復号・デコーダ接続は W3。

追加実測: HVF の通常 init / snapshot ディスク上で、ネイティブ CLI が
`cdm-host` ライブラリを直接使用して `--sessions 6` を完走した。
各要求の message は 1 件、1676〜1677 bytes、4 個の共有スロットを再利用。
6 回とも close が成功し、ホストは status=0 で終了した。
共有領域のレイアウト・制御フレーム長・イベントの欠損検証を加えた版でも
同じ 6 回のテストが成功した。実行記録は `/tmp/myrica-eme-serial.log`。

ブラウザ W2 の実測も成功 (WebKit pin `0e54210968a303fc1954c78333f2667dbc116c70`)。
通常 init / HVF / native video 有効の QEMU で、ホスト配信した fixture が
`secureContext=true`、6 回すべて `messages=1, usable=5` を返した。
大文字キーシステム名・HW 保護・CDM 内デコード・永続ライセンス・cbcs の
5 設定は `NotSupportedError`。`stage=widevine-eme` と
`stage=owner-joined` の両方が PASS、子ホストも status=0 で終了。
初期の localhost 検証ではゲスト内 fixture の配置漏れで 404 を見ていた。
ホスト配信に統一して解消した。

残る W3: MSE の暗号化メタデータと `encrypted` イベント、MediaKeys の
プレイヤー接続、復号後の Scarlet デコーダ入力。更新応答外に発生する
CDM の更新要求・期限・鍵状態変更も独立したイベント受信が必要。
初回ライセンス往復の成功は、これらの動作を保証しない。


## W3 メディア接続と実測 (2026-10-18)

WebKit `d92a622857607509cad2b6c9d0589c586610600d`、scarlet-codecs
`13cc8cff3c3a1ce02c51daff75ff60959563bfab`、Symphonia
`5865b5d0e5d170d1e0419684c04e8268847a9d2d` を使用。
MSE の PSSH が WebCore の `encrypted` イベントへ届き、ページの fetch が
ライセンスを取得する。MediaKeys の共有接続をメディアワーカーが保持し、
CENC メタデータ付き圧縮 packet を CDM に渡す。復号後だけ既存の
H.264/AAC デコーダへ渡す。WebCore の Page / compositor は CDM IPC を待たない。

鍵未到着時は packet とキューのバイト予算を保持する。同じ鍵世代での
復号再試行を止め、新しい鍵状態または MediaKeys の付け替えで再開する。
再生クロックは未復号 packet の PTS で止めるため、それ以前のデコード済み
フレームやクリアリードを再生できる。非同期の license message と鍵状態は
CDM WorkQueue で受信し、Page owner の標準 WebKit コールバックへ戻す。
通信エラー時はイベントポーリングを停止する。

HVF、通常 init、Linux ABI view の同じ CDM 4.10.2662.3、native video 有効で
ホスト配信した `tests/eme-widevine-playback.html` を Release ビルドで実行した。

| 入力 | ライセンス | 再生位置 / デコードフレーム | 結果 |
| --- | --- | --- | --- |
| Sintel 12〜16 秒、暗号化フラグメントのみ | message 1、usable 5 | 15.790664 秒 / 94 | PASS |
| Sintel 20〜24 秒、暗号化フラグメントのみ | message 1、usable 5 | 23.805997 秒 / 94 | PASS |
| 0〜16 秒、12 秒のクリアリードから暗号化へ遷移 | message 1、usable 5 | 15.791012 秒 / 382 | PASS |

暗号化フラグメントのみの試験は、クリアリードを再生して成功したと数える
ことを避ける。合格条件は最後の 0.25 秒以内まで再生し、4 秒区間で 90 枚以上、
16 秒区間で 378 枚以上をデコードすること。`?segment=20` と
`?segment=clear-lead` で入力を選べる。自動試験は DOM の状態とデコーダの
フレーム数を検査する。画面の画素比較と音声付き暗号化コンテンツは未検証。

検証用 HTTP サーバーはホスト側で実行する。

```sh
python3 tests/webkit_server.py --port 18770 --log /tmp/myrica-eme-http.jsonl
```

ゲストは `http_proxy=http://10.0.2.2:18770` を設定し、
`webkit-smoke http://localhost:18770/eme-widevine-playback.html --eme-only`
を実行する。localhost の secure context を通常の HTTP proxy 経由で
ホスト fixture に到達させる方式で、製品の TLS・origin 判定は変更しない。
CDM の配置は既存の `MYRICA_WIDEVINE_HOST` / `MYRICA_WIDEVINE_CDM` /
`MYRICA_WIDEVINE_LD_PATH` に従う。

他の検証: scarlet-codecs の通常 MP4 テスト、実 CENC fixture の 257 バイトずつの
増分入力 (clear 288 / encrypted 96 samples)、Symphonia の CENC / timeline
テスト、media_bridge の 28 テストが成功。Myrica / Vulkan probe / smoke の
Scarlet 向け Release ビルドが成功した。

残作業: `seig` によるフラグメント内鍵切替、非フラグメント暗号化 MP4、
`moov` 外 PSSH、暗号化音声との同期、再生中のシーク・鍵待ちの反復検証。
`seig`、cbcs/pattern encryption、保護された非フラグメント MP4 は明示的に拒否する。
非同期 expiry / spontaneous close の DOM 通知、ホスト異常終了のメディア通知も残る。
この時点で残っていたホスト内タイマーの単一スレッド化は、後述の Prime Video
調査中に実装・検証した。
実サービスごとの VMP / 出力保護要件への適合はこの fixture 試験の対象外。


### 公開 Shaka デモへの接続

MediaCapabilities の暗号化設定と `keySystemAccess` を上流 WebKit の
`NavigatorEME` へ接続した。Scarlet の codec 判定には既存の
`MediaPlayer::supportsType` を使う。性能測定に基づかない `smooth` /
`powerEfficient` の成功は返さない。Page 設定で MediaCapabilities を有効にした。
Widevine の service certificate も CDM の serial WorkQueue へ渡す。

実機の W2 fixture は clear / encrypted MediaCapabilities、未対応 robustness の
拒否、公開 service certificate の適用、6 セッションすべての
`messages=1, usable=5, close` と `owner-joined` を検査して PASS した。
セッションの終了後、GC から空の session id を HashMap に渡す不具合も修正した。

通常 Myrica で `https://shaka-project.github.io/shaka-player-release/demo/`
を直接開き、公開 Sintel Widevine MPD と `cwip-shaka-proxy.appspot.com/no_auth`
のライセンスを使用した。ライセンス要求 1 回、usable キー 5 個、
12 秒以降の暗号化区間への遷移と native デコーダのフレーム進行を確認した。
Shaka の正式な詳細設定で video / audio robustness を
`SW_SECURE_CRYPTO` に指定する。Shaka の既定の映像要求
`SW_SECURE_DECODE` は、この decrypt-only 経路の対応外である。

上流の navigator.vendor が Apple のままだと、Shaka が Scarlet を
WebKit TV/STB と判定して MediaCapabilities の別経路へ切り替えていた。
Scarlet の vendor を `Scarlet` にした。設定変更前の `98df2c09` 版で
上記再生を確認し、vendor 修正を含む `d92a6228` 版は Release ビルド済み。
`SCARLET_WEBVIEW_PROFILE` 有効時には、実際に復号した圧縮サンプルの数と
バイト数を最初の 1 回と 128 サンプルごとに表示する。
鍵・ライセンス応答・映像データはログへ出さない。

対話試験中のシーク・別アセット切替付近で、native video の
RESOURCE_QUEUE エラーと古い CDM 制御接続の終了を観測した。
操作と自動計測が重なったため、ABR 単独の障害とは判定していない。
その後に選ばれた別の動画は、追加の Widevine 要求がなく、
その再生結果は暗号化コンテンツの成功件数に含めない。
Amazon Prime Video / Netflix など実サービスのライセンス条件は未検証。

通常バンドルの `/bin/myrica` は、検証用 `/shared/myrica-eme-98df` と
異なる旧 pin のまま起動した記録がある。二つの実行ファイルを混同して
DRM 判定サイトの結果を評価しない。通常バンドルの更新は次の反映作業。


### 判定サイトと起動中 VM への反映

実装コミット `455279611ac954bb56084e7e1337ae75a230075c` の Release 実行ファイルで
`https://scrapfly.io/web-scraping-tools/drm-capabilities` を直接開いた。
同サイトの EME 判定は `'requestMediaKeySystemAccess' in navigator`。
その後の Widevine H.264/AAC capability 要求が受理され、robustness の個別検査
(`SW_SECURE_CRYPTO` を含む) に進んだことを WebCore の要求ログで確認した。
このサイトは capability 検査であり、ライセンス取得や復号の証拠には使わない。
復号・再生の検証対象は上記 Sintel の暗号化区間である。

起動中 VM への転送にはホスト上の一時 HTTP サーバーとゲストの curl を使った。
最初に Linux ビューの `/bin/myrica` を更新してしまい、native の
`/bin/myrica` を更新したと誤報した。Linux ツールのパスは Linux ビュー内で
解決される。`bootstrap::abi_view` が公開する `/scarlet` bind を使って、
`/scarlet/bin/myrica.widevine-new` に配置し、SHA-256 を照合した後に
`/scarlet/bin/myrica` へ rename して修正した。
当該ファイルの SHA-256 は
`4cc12bd36a7f47fdec2757cff8b1d10f6acc87d8a27ca9b55ec7a7c1422bf190`。
起動済みウィンドウの実行ファイルを truncate せず、次回起動を最新版にする。

Linux 側に誤配置した追加ファイルは共有の検証用パス
`/shared/myrica-eme-4552796` へ移動した。転送サーバーと fixture サーバーは
検証終了後に停止し、公開済みの WebKit / Symphonia 一時 checkout も削除した。
操作中の検証 VM とその使用中ディスクは保持している。
通常バンドルは実装コミット `4552796` を参照する。直後の作業記録コミット `348b46c` は
実行コードや WebKit pin を変えていない。


### ログイン画面のクリック調査

WebKit `821a50fb35f60d794a45e384e224e9f6ae10db95` では
`SCARLET_WEBVIEW_INPUT_TRACE=1` を指定した時だけ、mouse down / up の
到達先タグ、子フレームかどうか、関連 listener の有無を出す。
フォーム値、node id、URL、DOM テキストは記録しない。
通常時は追加の hit test を行わない。C++ syntax check と Myrica の Scarlet
Release ビルドが成功し、native `/bin/myrica` へ HTTP 転送・rename して配置した。

Amazon のパズルがクリックに反応しない件をユーザー操作で調査している。
通常ログでは当該操作時の新しい JS 例外はなかった。読み込み時には
`PublicKeyCredential.getClientCapabilities` の未対応警告と analytics endpoint の
CORS エラーがあるが、パズルの原因との対応は未確認。診断版の新しい Amazon
ウィンドウでユーザーが無反応を再現した。

最初の trace は native libc の浮動小数点書式で途中までしか出力されていなかった。
WebKit `d7e04060` で座標を整数にし、1 レコードをまとめて出力するよう修正した。
その版で、パズルの `CANVAS` に down / up が同じ座標で到達し、
canvas の click listener が存在することを確認した。新しい JS 例外はない。
`handled=0` は既定動作の処理結果であり、DOM listener が未実行という証拠ではない。

WebKit `ff38358b` は同じ opt-in flag の下で、入力を処理する間だけ
対象 document に受動的な capture listener を追加し、WebCore が実際に発火した
mousedown / mouseup / click の trusted、button、client / offset 座標、
canvas の内在サイズと CSS 矩形を記録する。listener は dispatch 後に外し、
ページの伝播・既定動作・フォーム値には触れない。syntax check と Scarlet
Release ビルドを通し、native `/bin/myrica` へ HTTP 転送して配置した。

その起動後、クリックの trace が届く前に Scarlet 全体のハングが報告された。
QMP の読み取りで CPU 0 が `rust_begin_unwind` 内の停止ループ
(`0xffffffff8013c810`) に留まり、他 CPU が idle になっていることを確認した。
起動した Release カーネルのシンボルと kernel stack 上の PanicInfo を照合し、
`vm::user_vm_init` (当時の 393 行目) の
`No free kernel stack window slots` を特定した。
全タスク共通の枠は `KERNEL_KSTACK_SLOTS=256` で、spawn 時の枯渇を
カーネル panic に変換する実装が残っている。枯渇に至ったタスクの内訳と、
元の Canvas 無反応との関連は未確定。カーネル修正はまだ行っていない。
ユーザー指示で再起動し、HVF / VirGL / native video を維持して
通常イメージのコピーから復帰した。HTTP で同じ診断版と CDM host の環境を復元した。


### Canvas と入力表示の更新欠落の修正 (2026-10-18)

WebKit `cb9e9527` で、Skia Canvas2D の delegated display 通知を
`RenderLayerBacking` の CanvasPixels 更新へ接続した。
Scarlet は `USE(SKIA)=1`、WebGL / OffscreenCanvas は無効のため、従来の
WebGL / OffscreenCanvas のみの条件では Canvas の更新通知が消えていた。
この版を native `/bin/myrica` に HTTP 転送し、ユーザー操作で
Amazon のパズルがクリックに反応することを確認した。

続く文字・入力表示の更新欠落は、別の非同期合成の競合だった。
新しい DOM タイルを worker が記録中でも、video / async scroll の
composition-only フレームが DOM damage を先に消費していた。
WebKit `a462f289` はこの damage を RenderingUpdate のタイル commit まで保持する。
スクロール位置の更新は、既存の position damage を使う。
調査用に一時導入した source / tile pixel readback は除去済み。

WebKit `55677b5184c7af6b7c723c2a50d3a11c67d69b7b` を使う Scarlet Release 版で、
以下を HVF / VirGL の検証 VM 内で実行した。

- `webkit-vulkan-probe --canvas-input`: DPR2 の native trusted click の座標、
  GPU / willReadFrequently Canvas の赤→緑、native 文字入力 ABC の入力欄と
  DOM テキストへの反映を、実際の GPU 出力の readback で確認した。
- `webkit-vulkan-probe --compositor-only`: CSS blur / drop-shadow、transform / opacity、
  小・大スクロールのタイル再利用と露出描画、idle の不要描画なし、navigation / resize
  と生存中 lease、DPR2 filter 境界、pending cover の隠蔽・再表示が通った。
- 両コマンドの初期チェックで TLS / errno の 4 thread 分離、Skia Vulkan の
  create / draw / readback / destroy 3 周、I420 / NV12 色変換と video presentation
  feedback の GPU テストも通った。

引数なしの旧 probe は「2 lease で capacity=0」を前提としており、現在の
3 slot 出力と一致せず停止する。上記の独立したテストを実行した結果であり、
旧 probe 全体の成功は記録していない。

ユーザーはログイン後の Prime Video 作品画面まで進んだが、再生ボタンを
押しても画面が変わらないと報告した。稼働中のウィンドウは paint profile 無効の
ため、EME 要求ログの不在から EME 未到達を推論しない。
WebKit `55677b51` は `SCARLET_EME_TRACE=1` 単独で capability 要求と
key-system / configuration の受理・拒否を記録できる。
フォーム値・ライセンス・鍵は記録しない。ログイン中のウィンドウを保持して次の診断を行う。
Cookie の永続化は WebCore の ScarletStorage / CookieJarDB が担当し、
HOME 以下の .local/share/myrica/cookies.sqlite を使う。ディレクトリ作成に失敗すると
メモリに退避する。Blitz 側の in-memory jar を根拠に、新しい WebKit process は
常に再ログインが必要と説明した点は誤りだった。


### Prime Video: EME 前のプレイヤー起動拒否

`SCARLET_EME_TRACE=1` の修正版でもユーザー操作で再現した。
同ウィンドウは `DVWebNode_deeplink` の `Unhandled Playback` を記録し、
`launchTypeToApply` は `NONE`。EME 要求は記録されず、別途 player が
20 秒後も `Unloaded` という timeout を出した。

認証情報を含まない公開 storefront をホストから取得して比較したところ、
標準の Myrica UA では JSON の `playbackLaunchType` が `NONE`、
Linux Chrome UA と desktop WebKit UA では `EMBEDDED` だった。
さらに `Mozilla/5.0 (X11; Scarlet aarch64) AppleWebKit/605.1.15 (KHTML, like Gecko) Myrica/0.1`
でも `EMBEDDED` を確認した。公開 client の device 分類は `X11` token を
Linux desktop と判定し、既存の Scarlet UA は Other へ分類する。
クリック handler は `EMBEDDED` の場合に onPlay を呼ぶが、`NONE` では
上記 Unhandled Playback を記録する。この結果はライセンス・復号の検証とは別である。

WebKit `0e3aa321` は明示的な `SCARLET_WEBVIEW_USER_AGENT` override を追加した。
標準 UA は保持し、navigator と WebCore HTTP request の双方に同じ指定を使う。
空文字・不正 UTF-8・1024 文字超・改行を含む値は採用しない。
互換 token は desktop 用 UI を選ばせるための試験設定で、EME capability の
応答を変えたり、CDM の security level を追加したりする変更はない。


最初の desktop WebKit 互換指定では、ユーザー操作で黒いプレイヤー画面へ
遷移するようになったが、20 秒でタイムアウトして視聴不可を表示した。
SDK の公開コードを確認したところ、`Safari` / `AppleWebKit` 判定では
MediaKeyFactoryWebkit (supportedKeySystems は com.apple.fps のみ) を選んでいた。
Chrome 判定では W3C EME の Widevine factory を選ぶ。
単に desktop に分類される UA だけでは、この DRM 分岐の相違が残る。
既定 UA を変更せず、明示的な Chrome/Linux 互換指定で切り分けを継続する。


Chrome/Linux 互換指定で再起動し、Prime Video の W3C EME 要求と
`SW_SECURE_CRYPTO` / 空 robustness の構成受理を確認した。
この段階で CDM host が `dlopen failed: libnspr4.so` と終了していた。
再起動後の検証 VM に CDM と NSPR を /shared/widevine 以下へ復元していたが、
MYRICA_WIDEVINE_LD_PATH を指定していなかったため探索対象外だった。
Linux ビューの /usr/lib/aarch64-linux-gnu/libnspr4.so が未配置であることを確認し、
手元の検証用依存ライブラリを HTTP 転送して SHA-256 を照合した。
CDM 本体の改変や能力応答の追加は行っていない。
私有 CDM / NSPR の検証用配置は起動中 VM の snapshot 内だけである。
別の VM では NSPR を通常の Linux runtime に導入するか、
MYRICA_WIDEVINE_LD_PATH で同梱ディレクトリを明示する。

途中の 1 回は Myrica が SWS surface 作成の失敗で終了した。
SWS は WINDOW_CREATED 送信後に window 作成を記録し、直後に client が切断した。
ScarletUI main `332caaf0` は元の sws-client エラー種別を出力する。
この版の Release ビルドと転送後の再起動は成功し、GUI / WebKit の起動完了を確認した。
同じ surface エラーは再現せず、原因修正済みとは扱わない。


NSPR 配置後の再起動では CDM 4.10.2662.3 が initialize を完了し、
Prime Video の license-request に応答が返り、3 鍵の通知と usable key 追加のフラグを受け取った。
SDK は hasStartedPlaying=yes を記録したが、その後 error 7031 の画面に遷移した。
公開 SDK のコードでは 7031 は xp.unknown に対応する汎用エラーであり、
この番号からネットワーク、HDCP、デコーダなど特定の原因を推論しない。
/dev/video0 の 1920x1080 H.264 configure / flush が記録された。
最初の映像が広告等の clear content か暗号化区間かは未確認。

WebKit `141519af` は SCARLET_EME_TRACE でも既存の低頻度 media lifecycle を
記録できるようにし、MSE append/parser の失敗を記録する。
Rust bridge も、従来詳細が消えていた音声出力、映像出力の容量・形式、
playbackRate / seek の不正値の失敗を記録する。
高頻度の paint / GPU readback 診断は有効にしない。


音声 pump の失敗は、検証 QEMU 起動時の SCARLET_QEMU_AUDIO 未指定による
音声デバイス不在だった。検証設定の漏れを修正した。
稼働中 snapshot の boot disk を pause / commit して私有の検証用コピーへ保存し、
通常 runner の SCARLET_QEMU_AUDIO=1 / coreaudio で再起動した。
virtio-snd / audio0 の認識、SAS の稼働、更新バイナリと CDM/NSPR の保持を確認した。
再試験では audio-pump エラーが消えたが、7031 は残った。
ネイティブ decoder / MSE append の失敗ログも出ていない。

host の usable=true は has_additional_usable_key であり、通知された全鍵が
usable という意味ではない。鍵 ID を出さず状態ごとの件数を記録して確認する。
SCARLET_EME_TRACE だけで復号した圧縮サンプル数と鍵待ちも記録する。

### Prime Video: 鍵状態と復号入力の切り分け

音声デバイスを有効にした同じ VM でユーザーが再現した結果、最初の通知は
usable 2 / pending 1、その後 usable 1 / internal-error 1 / pending 1 になった。
output-restricted / output-downscaled は 0。圧縮サンプル 341 バイトの復号は
1 件成功したが、これだけでは暗号化された映像の再生成功を示さない。

SCARLET_EME_TRACE に復号失敗の入力サイズだけを追加した。
停止した入力は 184 バイト、鍵 ID 16 バイト、IV 8 バイト、subsample 1 件、
clear prefix 50 バイトで、CDM の戻り値は NoKey。
鍵 ID・IV の値、ライセンス、復号済みデータは記録しない。
Host_10 の KeyInformation.system_code は従来捨てていたため、
usable / pending 以外の状態についてホストのエラーログに残す。
このコードは診断情報であり、鍵状態や EME capability の応答を変更しない。

単一 subsample に限り clear prefix を保持して CDM に渡す変更も比較したが、
Prime Video の 7031 は変わらなかった。公開 Sintel の暗号化区間も
12.083333 秒 / decoded 2 の位置で NoKey 待ちになったため、この変更は撤回した。
従来の cipher stream gather / scatter を維持する。
撤回後の同じ Release 実行では公開 Sintel の 12〜16 秒の暗号化区間が
15.844064 秒 / decoded 96 / usable 5 で合格し、owner の終了・join も完了した。
試験中、ホストの std::system_error / Operation not permitted による終了も記録された。
タイマーの別スレッド呼び出しとの因果関係は未確認。

7031 の原因をライセンスの商用ポリシー、出力保護、ネットワークのいずれかへ
断定する証拠は得られていない。次の再現では CDM の system_code を確認する。

### Prime Video: system_code=5 と CDM ホストの直列化

通常の gather / scatter を使う版でユーザーが 7031 を再現した。
3 鍵すべて usable の通知後、1 鍵が internal-error / system_code=5 へ変わり、
残る 2 鍵は usable。停止サンプルは 46461 バイト、IV 8 バイト、subsample 1 件、
clear prefix 49 バイトで NoKey。CDM 公開 ABI は system_code の 5 の意味を
定義しておらず、CDM の status enum やサーバーの HTTP エラー番号と混同しない。
別途、同ウィンドウのログには native decode の RESOURCE_QUEUE 失敗があり、
vhost-video は VideoToolbox callback -12909 を記録した。両エラーを残して調査する。

ホストの SetTimer が detached thread から TimerExpired を呼び、要求ループの
Decrypt / UpdateSession と並行していた。QueryOutputProtectionStatus /
RequestStorageId も CDM 呼び出しの内側から同期で CDM に再入していた。
これらを要求ループ上の期限付き task queue に統合した。
制御ソケットの idle / 部分読み取り中、initialize / promise 応答待ちの間も
タイマーを実行する。別スレッドからの通知は nonblocking wake socket で受ける。
終了時は残りの callback を破棄し、Destroy 後に TimerExpired を呼ばない。
未提供の platform attestation は空の失敗応答を返し、要求を放置しない。
robustness・出力保護・鍵状態の成功値を追加する変更はない。

task runner の owner thread / timer 順序 / nested callback / 他スレッド wake /
部分 stream / EOF / timeout / 終了時 cancellation テストが macOS と aarch64 Linux
container で合格した。Scarlet の同じ CDM を使った Release の公開 Sintel 試験も
15.765890 秒 / decoded 94 / usable 5 で合格し、owner join と host 終了を確認した。
この修正による Prime Video の再生結果は次のユーザー操作で検証する。

再現後も internal-error / system_code=5 と映像先頭の NoKey は残った。
別の圧縮サンプルの復号カウントは 384 件以上に進んだが、画面の再生位置は
0:00 のまま黒画面だった。タイマー競合の修正だけではこの鍵エラーは解消しない。

途中のブラウザ終了は、CDM host の例外ではなく GPU の受付上限超過だった。
VirGL は staged=67056408 / packet=65452 / limit=67108864、Vulkan は
texture_bytes=88109682 / passes=116 / draws=337 を記録し、メモリ不足から
device lost に遷移した。CDM host はその後の制御ソケット切断で終了した。
SGFX の既存 upload_batches は texture-only の command buffer に限って分割し、
render pass が混在すると総転送量が多くても一括送信していた。
SGFX `791e76f659f9ecdfafaa20bb5b66e261589d9a91` で、render/compute pass の内部を
保ったまま前後の upload を 8 MiB ずつ分割する。全体と分割後の両方を事前検証し、
転送順序・subrectangle・mip/layer・buffer offset を保持する。
80 MiB + render pass の回帰試験を含む 6 件、Vulkan 単体 117 件が合格した
(GPU が必要な 4 件は既存の ignored)。実機反映後の検証を続ける。

通常バンドルの Release ビルドを完了し、rootfs 内の Vulkan ICD と VM へ転送した
ファイルの SHA-256 が一致した (385a798672c953b1fe4ca0844c99e46e9d7f6f9acdb058b96b492426c3abc6dc)。
この ICD と直列化した CDM host を組み合わせ、公開 Sintel の暗号化区間は
15.768925 秒 / decoded 94 / usable 5、owner join まで合格した。
Prime の再試験用ウィンドウも同じ ICD で起動した。

復号時の鍵 ID が CDM 通知済みの鍵かを照合し、失敗時に matching_sessions /
usable / internal_error / system_code の件数を出す診断を追加した。
実際の鍵 ID をログに出さず、session が閉じたら照合用状態も破棄する。
これは復号先や鍵の選択を変更する機能ではない。

GPU 修正後もユーザー操作で Prime の 7031 が再現した。失敗サンプルは
matching_sessions=1 / usable=0 / internal_error=1 / system_code=5。
復号に渡す鍵 ID は CDM が通知した鍵と一致している。ライセンス受信前の
matching_sessions=0 と、受信後に鍵自体がエラーとなる状態を区別できた。
同試験で SGFX の受付上限超過 / device lost は記録されなかった。

### CDM ホスト応答の監査

WebKit は空 robustness / SW_SECURE_CRYPTO のみを公開し、hardware secure /
CDM 内 decoder の能力を公開していない。保存不可時の CreateFileIO=null /
StorageId=empty は Chromium の実装とも一致する。
Chromium の CDM host verification の既定対象は branded Windows / Mac であり、
署名不足を Linux 経路の原因と断定しない。

一方、Scarlet ホストの QueryOutputProtectionStatus は表示経路の検査を
実装せず kQuerySucceeded / kLinkTypeNone を固定応答していた。
取得不能として kQueryFailed / kLinkTypeUnknown / kProtectionNone を返すよう修正した。
EnableOutputProtection は要求 mask と未対応であることを記録する。
出力保護を有効にしたとの応答や、CDM の鍵状態を書き換える処理は追加しない。
同 CDM が mask=1 を要求・query を行った公開 Sintel の試験でも、修正後は
15.785546 秒 / decoded 94 / usable 5、owner join まで合格した。
Prime への影響はページを再読み込みして再試験する。

参照: [Chromium CdmAdapter](https://github.com/chromium/chromium/blob/main/media/cdm/cdm_adapter.cc)
の RequestStorageId / CreateFileIO / OnQueryOutputProtectionStatusDone、
[media_options.gni](https://github.com/chromium/chromium/blob/main/media/media_options.gni)
の enable_cdm_host_verification。

### Prime Video の SD 再生を目標に切り分け

ユーザーは SD 画質での再生を希望した。HDCP の対応範囲を拡大する作業とは分け、
まず選択された暗号化映像の解像度を実測する。
従来の width / height 記録はデコーダ初期化後だけだったため、復号前に NoKey で
停止する映像には出なかった。既存の低頻度 SCARLET_MEDIA_KEY_WAIT に
codec / width / height を追加した。認証情報・鍵 ID・ライセンスは記録しない。
Scarlet 向け Release ビルドで確認した。

Amazon の公式要件は外部出力の HD に HDCP 1.4、UHD/HDR に HDCP 2.2 と記載し、
PC 要件では Windows/macOS 以外を標準画質に制限している。
これだけで SD の全コンテンツが出力保護を要求しないとは判断しない。
現在の Chrome/Linux 互換 UA が選ぶ実際の画質と、鍵状態を照合する。

参照: [再生時の公式ヘルプ](https://www.primevideo.com/help?nodeId=GU85HKX66NVFNQ9Y&language=ja_JP)、
[PC の公式要件](https://www.primevideo.com/help?nodeId=GUX9FYHU5D8LC9EJ&language=ja_JP)。

ユーザー操作で選択された H.264 track は 960x540 (720p 未満)、先頭 PTS は
644477166 us だった。同じ映像の鍵は internal-error / system_code=5 で NoKey が続く。
後から別の鍵に output-restricted / system_code=49 も現れた。復号に使った鍵の
照合結果は internal-error のままであり、全鍵を HDCP エラーと一括分類しない。

Chromium の MP4 parser / DecryptConfig / ToCdmInputBuffer と照合すると、
Scarlet host は 8 バイト IV を CDM に直接渡していたが、Chromium は
後半 8 バイトをゼロ埋めして 16 バイトに正規化していた。
host の CENC InputBuffer 組み立て時に同じ正規化を追加した。
16 バイト IV、鍵、subsample の暗号化 stream 順序は保持する。
CDM に渡した実際の IV 長だけを復号失敗ログに残す。
同じ Scarlet/CDM の公開 Sintel 暗号化区間は 15.778688 秒 / decoded 94 / usable 5、
owner join まで合格した。Prime の低解像度映像での影響は次の再試験で確認する。

参照: [Chromium TrackRunIterator の IV 正規化テスト](https://chromium.googlesource.com/chromium/src/+/45ec87b29fb536932d3c26d6cf236128a872a14b/media/formats/mp4/track_run_iterator_unittest.cc)、
[DecryptConfig](https://chromium.googlesource.com/chromium/src/media/+/refs/heads/main/base/decrypt_config.h)、
[ToCdmInputBuffer](https://chromium.googlesource.com/chromium/src/+/HEAD/media/cdm/cdm_type_conversion.cc)。

IV 正規化後の Prime 再試験でも、ユーザー操作では映像が出なかった。
host の実際の IV 長は 16、H.264 は 960x540、最初の映像サンプルは
141052 バイト (clear prefix 49 バイト) だった。
matching_sessions=1 / internal_error=1 / system_code=5 の鍵で NoKey が続く一方、
圧縮音声サンプルは 384 件まで復号が進んだ。後続の別鍵には
output-restricted / system_code=49 も出た。IV の差は修正できたが、
Prime の映像再生失敗は継続している。低解像度を選ぶだけでは改善しなかった。
この再試験範囲には SGFX の受付上限超過 / device lost の記録はない。
