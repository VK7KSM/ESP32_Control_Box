# elfRadio BOX — 上位机使用说明

业余无线电控制盒 PC 上位机，通过 USB 串口与 ESP32-S3 控制盒通信，实现对 TYT TH-9800 电台的软件控制。

- **台长呼号**：VK7KSM
- **串口连接**：自动检测 Espressif USB VID（0x303A），或通过 `--port` 手动指定（如 `COM3`、`COM8` 等，以设备管理器实际显示为准）
- **通信协议**：`[0xAA][0x55][Type][LenLo][LenHi][Payload][CRC16-CCITT]`

---

## 快速开始

### 编译

```bash
cd /c/eh/examples/elfradio-box
cargo build
```

输出：`target/x86_64-pc-windows-msvc/debug/elfradio-box.exe`

### 运行方式

| 方式 | 行为 |
|---|---|
| **双击 exe** | 自动检测串口 → 显示主菜单 |
| **PowerShell / CMD** | 支持完整 CLI 指令（见下文）|

---

## TUI 监听界面

双击或运行 `elfradio-box.exe` 后，选择 **[1] 监听模式** 进入 TUI 实时界面。

也可通过 CLI 直接进入，跳过主菜单：

```
elfradio-box.exe --port <串口号> monitor
```

### TUI 界面说明

```
elfRadio  RIGHT MAIN  FM 433.550.000 MHz  HIGH  S3          OK 监听中  空闲
──────────────────────────────────────────────────────────────────────────────
（接收消息区：BUSY 录音保存记录等）




──────────────────────────────────────────────────────────────────────────────
L:左频率 R:右频率  M:切MAIN  P:长按发射  O:开关机  ↑↓:旋钮  V+←→:音量  Q+←→:静噪  W+↑↓:功率  T+↑↓:亚音  Esc:退出
Radio:OK PC:OK  L: VOL 60 % / SQL 25 %  R: VOL 0 % / SQL 0 %
```

### TUI 键盘操作

| 按键 | 功能 |
|---|---|
| `L` | 输入 LEFT 侧频率（6 位数字，自动切 MAIN）|
| `R` | 输入 RIGHT 侧频率（6 位数字，自动切 MAIN）|
| `M` | 切换 MAIN 侧（LEFT ↔ RIGHT）|
| `P`（长按 0.3s）| PTT 发射，松开停止；最长 30 秒后自动停止（与 ESP32 看门狗同步）|
| `O` + `Y` | 电台开关机（GPIO8 脉冲）|
| `↑` / `↓` | MAIN 侧旋钮步进（调频）|
| `V` + `←` / `→` | 音量 ±5% |
| `Q` + `←` / `→` | 静噪 ±5% |
| `W` + `↑` / `↓` | 功率循环（LOW → MID → HIGH）|
| `T` + `↑` / `↓` | 亚音模式循环（OFF → ENC → T/R → DCS）|
| `Esc` | 返回主菜单 |

### 状态栏说明

- 右上角：`OK 监听中  空闲` / `RX 接收中`（蓝底）/ `TX 发射中 30s`（红底，含倒计时）
  - TX 倒计时每秒更新（30→29→...→1），30 秒到自动停止发射
- 底部：`Radio:OK` 电台在线 / `PC:OK` 上位机在线 / 左右侧 VOL/SQL 百分比

### 自动录音

检测到 USB Audio 输入设备（连接电台扬声器输出）时自动启用。  
信号持续超过 0.3 秒后保存为 `recordings/` 目录下的 WAV 文件（48kHz 单声道）。

---

## CLI 命令行

### 串口说明

程序启动时**自动检测** Espressif USB 设备（VID 0x303A）。若检测失败或有多个串口，使用 `--port` 手动指定：

```
elfradio-box.exe --port <串口号> <命令>
```

串口号以 Windows **设备管理器 → 端口（COM 和 LPT）** 中显示的为准（如 COM3、COM9 等）。

> 以下示例中 `<串口号>` 请替换为实际串口号（如 `COM3`、`COM9`），或省略 `--port` 选项让程序自动检测。

### 用法格式

```
elfradio-box.exe [--port <串口号>] <命令> [参数...]
```

**全局选项**：
- `--port <PORT>` — 指定串口，省略则自动检测 Espressif VID（0x303A）

---

### 命令参考

#### 帮助

```
elfradio-box.exe help
```

打印完整帮助信息，**无需连接串口**，直接退出。

---

#### 进入监听模式

```
elfradio-box.exe --port <串口号> monitor
```

跳过主菜单，直接进入 TUI 实时监听界面。按 `Esc` 退出。

---

#### 频率设置

```
elfradio-box.exe --port <串口号> set-freq <L|R> <NNNNNN>
```

- `L` / `R`：左侧或右侧
- `NNNNNN`：6 位纯数字，单位 kHz，**无小数点**

自动切换 MAIN 到目标侧后逐位输入频率，等待电台响应后读取确认频率。

**示例**：

```bash
# 设置 LEFT 频率为 433.550 MHz
elfradio-box.exe --port <串口号> set-freq L 433550

# 设置 RIGHT 频率为 145.500 MHz
elfradio-box.exe --port <串口号> set-freq R 145500

# 设置 RIGHT 频率为 146.525 MHz（全国呼叫频率）
elfradio-box.exe --port <串口号> set-freq R 146525
```

---

#### 切换 MAIN 侧

```
elfradio-box.exe --port <串口号> main <L|R>
```

将 MAIN（主操作侧）切换到 LEFT 或 RIGHT。若已经是目标侧则直接返回成功。

```bash
elfradio-box.exe --port <串口号> main L
elfradio-box.exe --port <串口号> main R
```

---

#### 旋钮步进

```
elfradio-box.exe --port <串口号> knob <up|down> [N]
```

对 MAIN 侧进行旋钮步进，`N` 为步进格数（默认 1，最大 50）。  
步进后读取并显示当前频率。

```bash
# 频率上调 1 格（约 12.5kHz）
elfradio-box.exe --port <串口号> knob up

# 频率下调 5 格
elfradio-box.exe --port <串口号> knob down 5

# 上调 10 格（~125kHz）
elfradio-box.exe --port <串口号> knob up 10
```

---

#### 音量

```
elfradio-box.exe --port <串口号> set-vol <0-100>
```

设置音量百分比（0 = 最小，100 = 最大）。

```bash
elfradio-box.exe --port <串口号> set-vol 60
elfradio-box.exe --port <串口号> set-vol 0    # 静音
```

---

#### 静噪

```
elfradio-box.exe --port <串口号> set-sql <0-100>
```

设置静噪门限百分比（0 = 全开，100 = 最紧）。

```bash
elfradio-box.exe --port <串口号> set-sql 25
elfradio-box.exe --port <串口号> set-sql 0    # 关闭静噪（全噪）
```

---

#### 功率等级

```
elfradio-box.exe --port <串口号> set-power <L|R> <low|mid|high>
```

设置指定侧的发射功率。程序循环按 LOW 键直到达到目标功率，最多循环 5 次。

```bash
elfradio-box.exe --port <串口号> set-power L low    # LOW 约 5W
elfradio-box.exe --port <串口号> set-power L mid    # MID 约 10-25W
elfradio-box.exe --port <串口号> set-power L high   # HIGH 约 50W
```

> **注意**：TH-9800 功率档位为 LOW/MID/HIGH（状态报告中为 "LOW"/"MID"/"HIGH"）。

---

#### 亚音模式循环

```
elfradio-box.exe --port <串口号> tone
```

循环切换亚音模式（每次发送 P3 键）：`OFF → ENC → T/R → DCS → OFF → ...`

```bash
elfradio-box.exe --port <串口号> tone    # 切换一次
elfradio-box.exe --port <串口号> tone    # 再切换一次
```

---

#### PTT 发射（定时）

```
elfradio-box.exe --port <串口号> ptt <秒数>
```

PTT 发射指定秒数后自动释放（范围 **1-30 秒**）。  
**上限 30 秒**：ESP32 固件内置看门狗，超过 30 秒强制切断发射，防止电台过热或长时间占用频率。发射期间每秒打印倒计时。

```bash
# 发射 5 秒后自动停止
elfradio-box.exe --port <串口号> ptt 5

# 发射 10 秒（如发送 CQ 调用）
elfradio-box.exe --port <串口号> ptt 10
```

---

#### 强制关闭 PTT

```
elfradio-box.exe --port <串口号> ptt-off
```

立即发送 PTT 释放命令。用于异常情况下 PTT 卡死时的紧急关闭。

```bash
elfradio-box.exe --port <串口号> ptt-off
```

---

#### PTT + 播放音频文件

```
elfradio-box.exe --port <串口号> ptt-tx <file.wav>
```

开启 PTT → 将 WAV 文件播放到 USB Audio 输出设备（需连接电台麦克风输入）→ 播完自动松开 PTT。

- 支持 WAV 格式（i16 或 f32）
- 自动重采样到设备原生采样率
- 音频输出到名称含 "USB Audio" 的设备

```bash
# 播放 CQ 呼叫录音进行发射
elfradio-box.exe --port <串口号> ptt-tx cq_call.wav

# 播放欢迎语音
elfradio-box.exe --port <串口号> ptt-tx welcome.wav
```

---

#### 电台开关机

```
elfradio-box.exe --port <串口号> power-toggle
```

触发 GPIO8 控制的 PC817 光耦脉冲（1.2 秒），模拟按下电台电源键。  
电台开机时执行此命令 → 关机；电台关机时执行此命令 → 开机。

```bash
elfradio-box.exe --port <串口号> power-toggle
```

---

## 错误处理

| 错误信息 | 原因 | 解决方法 |
|---|---|---|
| `打开 <串口号> 失败: 拒绝访问` | 串口被其他程序占用 | 关闭其他上位机实例或 miniterm |
| `未找到 ESP32 串口` | ESP32 未通电或驱动未安装 | 检查 USB 连接，查看设备管理器 |
| `未收到状态报告` | ESP32 固件未烧录或串口不对 | 检查固件版本，确认串口号 |
| `未找到 USB Audio 输出设备` | ptt-tx 需要 USB Audio | 检查 CM108 声卡连接 |
| `功率循环 5 次后未达到目标` | 电台通信异常 | 手动核查当前功率，重试 |

---

## 脚本示例

### 全自动播报（Shell 脚本）

```bash
#!/bin/bash
# 将 PORT 替换为实际串口号（设备管理器中查看，如 COM3、COM9）
PORT=<串口号>
EXE="./elfradio-box.exe"

# 设置频率和功率
$EXE --port $PORT set-freq L 433550
$EXE --port $PORT set-power L low

# 发射 CQ
$EXE --port $PORT ptt-tx cq.wav

# 等待 5 秒后再次发射
sleep 5
$EXE --port $PORT ptt-tx cq.wav
```

### 预设配置（批处理）

```bat
@echo off
:: 将 PORT 替换为实际串口号（设备管理器中查看，如 COM3、COM9）
set PORT=<串口号>
set EXE=elfradio-box.exe

echo 设置左侧：VHF 144MHz 中继频率
%EXE% --port %PORT% main L
%EXE% --port %PORT% set-freq L 145000
%EXE% --port %PORT% set-power L low
%EXE% --port %PORT% set-vol 70
%EXE% --port %PORT% set-sql 20

echo 完成！
```

---

## 注意事项

1. **串口独占**：同一时刻只能有一个程序打开串口。TUI 监听模式运行时不能同时执行其他 CLI 命令，否则会提示"拒绝访问"。
2. **PTT 安全**：`ptt <秒>` 命令上限 **30 秒**（ESP32 看门狗硬限制，超时强制切断）。超过 30 秒的发射需求请重复调用或使用 `ptt-tx` 播放音频文件。
3. **频率格式**：`set-freq` 仅接受 6 位纯数字（kHz）。433.550 MHz → 填 `433550`；145.000 MHz → 填 `145000`。
4. **功率循环方向**：TH-9800 LOW 键固定按 LOW→MID→HIGH 方向循环，`set-power` 最多需要 3 次按键。

---

## 开发信息

- **语言**：Rust（x86_64-pc-windows-msvc）
- **主要依赖**：`crossterm`（TUI）、`serialport`（串口）、`cpal`（音频）、`hound`（WAV）
- **固件仓库**：`C:\eh\`（ESP32-S3 固件，Xtensa Rust）
- **通信协议文档**：`C:\eh\CLAUDE.md` → "PC 上位机通信开发记录"

73 de VK7KSM
