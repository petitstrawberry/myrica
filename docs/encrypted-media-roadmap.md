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
- 重要な制約: この CDM ビルドは、サブサンプルマップが 2 件以上ある Decrypt 要求に kNoKey を返す。複数 NAL の映像サンプルは Decrypt では復号できないため、映像は DecryptAndDecode (VideoDecoder / VideoFrame コールバック) 経由にする必要がある。Chromium が音声に Decrypt、映像に DecryptAndDecode を使う分担と一致し、W3 の前提がこれで確定した。
- CENC 解析の知見 (scarlet-codecs に持ち込む): stsd は 8 バイト、encv は 78 バイト、enca は 28 バイトのヘッダの後に子ボックスが並ぶ。tenc にはパターン用バイトを含む 24 バイトのレイアウト (Shaka Packager など) と含まない旧レイアウトがあり、ボックスサイズで判別できる。senc は IV (tenc の default_Per_Sample_IV_Size、通常 8 バイト) とサブサンプル対 (clear, cipher) を持ち、サンプル位置は trun の data_offset から求まる。参照実装は tools/widevine-host/test/extract-sample.py。
- 映像経路の実測 (2026-10-18, 途中): この CDM ビルドでは DecryptAndDecodeFrame がフレームを返さない。クリアな H.264 (avcC 付き、増加するタイムスタンプ、512x288 High) を DecryptAndDecodeFrame に流しても全サンプルが kNeedMoreData のままだった。Decrypt はサブサンプルマップが 2 件以上で kNoKey、1 件なら成功する。サブサンプルごとに 1 件ずつの Decrypt 要求に分割する方式は、2 番目の NAL を単独で復号して NAL 長 6264 が復元できたことから「各暗号化領域は同じ IV で独立に復号できる」という観測に基づいて実装した。96 サンプル中 80 件は成功したが 16 件が kNoKey (単独実行でも再現)。映像の復号検証 (ffmpeg による参照デコード) は未実施。
- 映像復号の検証 (2026-10-18): サブサンプル単位に分割した Decrypt (各暗号化領域で同じ IV を使う) で Axinom セグメントの 96 サンプルを復号し、ffmpeg で再生したところ 2.0 秒地点のフレームが正しい絵 (Blender/Axinom のテスト映像、オーバーレイに H.264 High Profile 512x288 と表示) として復号できた。この方式の復号自体は正しい。ただしこの CDM は 96 サンプル中 16 件で不安定で、ある実行では kSuccess を返しながら出力をゼロで埋め、別の実行では同じサンプルに kNoKey を返す。DecryptAndDecodeFrame はクリアな H.264 でも一切フレームを返さない。
- 映像経路の結論 (暫定): この ChromeOS 向け CDM ビルドは映像の復号・デコード経路が我々の環境では信頼できない。設計としては「サブサンプル単位の Decrypt + Scarlet 側デコーダ」が正しい形なので、次は別の CDM ビルド (Chrome for Linux arm64 など) で同じ経路を比較する。ClearKey 経路はこの影響を受けない。
- 映像経路の選択肢: (a) 別の CDM ビルド (Chrome for Linux arm64 など) で同じ経路を試す、(b) サンプル全体が 1 つの暗号化領域であるストリームを対象にする (Decrypt がそのまま使える)、(c) 映像は ClearKey 経路に限定する。
- W1/W2 の橋渡し (2026-10-18): tools/widevine-host-client をライブラリ (cdm_host) と CLI に分割し、aarch64-unknown-scarlet でのビルドを確認した。ライブラリは共有メモリと socketpair の作成、linux-aarch64 ビューのホスト起動 (stdio を 0..2、socket を 3、共有メモリを 4 に map)、HELLO/READY、initialize / create_session / update_session / close_session / decrypt / shutdown を提供する。decrypt はサブサンプル単位に分割して同じ IV で呼び、ゼロ埋めの成功は 1 回だけ再試行する。
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
