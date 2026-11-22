//! ZMODEM（rz / sz）传输驱动。
//!
//! `zmodem2` 只提供无 I/O 的状态机：宿主负责把远端字节喂进 `submit_wire`，
//! 再把 `Action` 要的线上字节写回 PTY、把文件字节落到本地磁盘。这里把两套
//! 状态机（接收 / 发送）包成统一的 `Transfer`，并负责探测起始标记、
//! 选择下载目录、生成去重文件名。
//!
//! 方向判断：远端 `sz` 是发送方，首个头是 ZRQINIT(0x00) → 我方接收；
//! 远端 `rz` 是接收方，首个头是 ZRINIT(0x01) → 我方发送。

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

/// 取消传输时发给远端的 lrzsz `canit()` 序列（10 个 CAN + 10 个退格）。
pub const CANCEL: [u8; 20] = [
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08,
];

/// 传输方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// 远端 `sz` 发文件，本端保存（下载）。
    Download,
    /// 远端 `rz` 收文件，本端读取并发送（上传）。
    Upload,
}

/// 输出字节流的探测结果。
pub enum Probe {
    /// 没有发现 ZMODEM 起始标记。
    None,
    /// 疑似标记出现在 `from`，但头类型字节还没到齐，需要保留后续数据。
    Partial(usize),
    /// 命中完整标记：`from` 之前的字节是普通终端输出，之后属于协议流。
    Start { from: usize, direction: Direction },
}

/// 扫描 `**\x18A`（binary）或 `**\x18B`（hex）头，判断传输方向。
pub fn detect(bytes: &[u8]) -> Probe {
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'*' && bytes[index + 2] == 0x18 {
            let Some(kind) = bytes.get(index + 3) else {
                return Probe::Partial(index);
            };
            match *kind {
                b'A' => {
                    let Some(frame) = bytes.get(index + 4) else {
                        return Probe::Partial(index);
                    };
                    return classify(index, *frame);
                }
                b'B' => {
                    let (Some(high), Some(low)) = (bytes.get(index + 4), bytes.get(index + 5))
                    else {
                        return Probe::Partial(index);
                    };
                    let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low)) else {
                        index += 1;
                        continue;
                    };
                    return classify(index, high * 16 + low);
                }
                _ => {}
            }
        }
        index += 1;
    }
    Probe::None
}

/// 没有命中标记时，尾部有多少字节需要留到下一块——
/// 只保留恰好是 `**\x18` 前缀的后缀，避免普通输出被延迟两字节。
pub fn pending_suffix_len(bytes: &[u8]) -> usize {
    const MARKER: &[u8] = b"**\x18";
    let max = MARKER.len().min(bytes.len());
    for len in (1..=max).rev() {
        if bytes[bytes.len() - len..] == MARKER[..len] {
            return len;
        }
    }
    0
}

fn classify(from: usize, frame: u8) -> Probe {
    match frame {
        0x00 => Probe::Start {
            from,
            direction: Direction::Download,
        },
        0x01 => Probe::Start {
            from,
            direction: Direction::Upload,
        },
        _ => Probe::None,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 传给 UI / 状态栏的传输事件。
#[derive(Clone, Debug)]
pub enum TransferEvent {
    /// 一般提示（开始接收 / 已保存路径等）。
    Info(String),
    /// 会话结束。
    Finished,
    /// 出错或被取消。
    Aborted(String),
}

enum Role {
    Download(Receiver),
    Upload { sender: Sender },
}

/// 一次进行中的 rz / sz 会话。
pub struct Transfer {
    role: Role,
    download_dir: PathBuf,
    /// 接收时是输出文件，发送时是输入文件。
    file: Option<File>,
    current_path: Option<PathBuf>,
    pub events: Vec<TransferEvent>,
    done: bool,
}

impl Transfer {
    /// 新建下载接收端（远端 `sz`）。
    pub fn download() -> Result<Self, String> {
        Self::download_in(download_dir())
    }

    fn download_in(download_dir: PathBuf) -> Result<Self, String> {
        let receiver = Receiver::with_flow_control(0, true).map_err(|err| err.to_string())?;
        Ok(Self {
            role: Role::Download(receiver),
            download_dir,
            file: None,
            current_path: None,
            events: Vec::new(),
            done: false,
        })
    }

    /// 新建上传发送端（远端 `rz`）。文件路径由用户稍后提供。
    pub fn upload() -> Result<Self, String> {
        let sender = Sender::new().map_err(|err| err.to_string())?;
        Ok(Self {
            role: Role::Upload { sender },
            download_dir: download_dir(),
            file: None,
            current_path: None,
            events: Vec::new(),
            done: false,
        })
    }

    pub fn is_upload(&self) -> bool {
        matches!(self.role, Role::Upload { .. })
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 喂入远端字节，返回需要写回 PTY 的字节。
    pub fn feed(&mut self, mut bytes: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        while !bytes.is_empty() && !self.done {
            let consumed = match &mut self.role {
                Role::Download(receiver) => receiver.submit_wire(bytes),
                Role::Upload { sender, .. } => sender.submit_wire(bytes),
            };
            match consumed {
                Ok(0) => {
                    // 对端背压：先把待发送的线上字节抽干，再重试同一段输入。
                    let before = wire.len();
                    wire.extend(self.pump());
                    if wire.len() == before {
                        break;
                    }
                }
                Ok(consumed) => {
                    bytes = &bytes[consumed..];
                    wire.extend(self.pump());
                }
                Err(err) => {
                    self.events
                        .push(TransferEvent::Aborted(format!("协议错误：{err}")));
                    self.done = true;
                    break;
                }
            }
        }
        if !self.done {
            wire.extend(self.pump());
        }
        wire
    }

    /// 用户填写本地路径后开始发送。返回需要写回 PTY 的字节。
    pub fn begin_upload(&mut self, path: &Path) -> Result<Vec<u8>, String> {
        if !self.is_upload() {
            return Err("当前不是上传会话".to_owned());
        }
        let mut file =
            File::open(path).map_err(|err| format!("打开 {} 失败：{err}", path.display()))?;
        let size = file.metadata().map_err(|err| err.to_string())?.len();
        let size =
            u32::try_from(size).map_err(|_| "文件过大，ZMODEM 只支持 4 GiB 以内".to_owned())?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload.bin".to_owned());
        let mut info = FileInfo::new(name.as_bytes(), Some(Position::new(size)));
        info.mode = Some(0o100_644);
        let Role::Upload { sender } = &mut self.role else {
            return Err("当前不是上传会话".to_owned());
        };
        sender
            .start_file(info)
            .map_err(|err| format!("启动上传失败：{err}"))?;
        let _ = file.seek(SeekFrom::Start(0));
        self.file = Some(file);
        self.current_path = Some(path.to_path_buf());
        self.events
            .push(TransferEvent::Info(format!("正在发送 {}…", path.display())));
        Ok(self.pump())
    }

    /// 拉取状态机当前待办，直到 Idle；返回要写回 PTY 的字节。
    fn pump(&mut self) -> Vec<u8> {
        let mut wire = Vec::new();
        loop {
            let work = match &mut self.role {
                Role::Download(receiver) => match receiver.poll() {
                    Action::WriteWire(data) => Work::Wire(data.to_vec()),
                    Action::WriteFile(data) => Work::File(data.to_vec()),
                    Action::Event(event) => Work::Event(OwnedEvent::from(event)),
                    _ => Work::Idle,
                },
                Role::Upload { sender, .. } => match sender.poll() {
                    Action::WriteWire(data) => Work::Wire(data.to_vec()),
                    Action::ReadFile { offset, max_len } => Work::Read(offset.get(), max_len),
                    Action::Event(event) => Work::Event(OwnedEvent::from(event)),
                    _ => Work::Idle,
                },
            };
            match work {
                Work::Wire(data) => {
                    let len = data.len();
                    wire.extend_from_slice(&data);
                    match &mut self.role {
                        Role::Download(receiver) => receiver.wire_written(len),
                        Role::Upload { sender, .. } => sender.wire_written(len),
                    }
                }
                Work::File(data) => {
                    let len = data.len();
                    let written = match self.file.as_mut() {
                        Some(file) => file.write_all(&data).and_then(|()| file.flush()),
                        None => Err(std::io::Error::other("目标文件未打开")),
                    };
                    match &mut self.role {
                        Role::Download(receiver) => {
                            let _ = receiver.file_written(len);
                        }
                        // 发送端不会要求写文件。
                        Role::Upload { .. } => {}
                    }
                    if let Err(err) = written {
                        self.abort_role();
                        self.events
                            .push(TransferEvent::Aborted(format!("写入文件失败：{err}")));
                        self.done = true;
                        break;
                    }
                }
                Work::Read(offset, max_len) => {
                    let chunk = self.read_source(offset, max_len);
                    let Role::Upload { sender, .. } = &mut self.role else {
                        break;
                    };
                    match chunk {
                        Ok(chunk) if !chunk.is_empty() => {
                            if let Err(err) = sender.submit_file(&chunk) {
                                self.events
                                    .push(TransferEvent::Aborted(format!("读取文件失败：{err}")));
                                self.done = true;
                                break;
                            }
                        }
                        Ok(_) => {
                            self.events.push(TransferEvent::Aborted(
                                "本地文件在发送过程中变短了".to_owned(),
                            ));
                            self.done = true;
                            break;
                        }
                        Err(err) => {
                            self.events
                                .push(TransferEvent::Aborted(format!("读取文件失败：{err}")));
                            self.done = true;
                            break;
                        }
                    }
                }
                Work::Event(event) => self.on_event(event),
                Work::Idle => break,
            }
        }
        wire
    }

    fn read_source(&mut self, offset: u32, max_len: usize) -> Result<Vec<u8>, String> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| "本地文件未打开".to_owned())?;
        file.seek(SeekFrom::Start(u64::from(offset)))
            .map_err(|err| err.to_string())?;
        let mut chunk = vec![0; max_len];
        let read = file.read(&mut chunk).map_err(|err| err.to_string())?;
        chunk.truncate(read);
        Ok(chunk)
    }

    fn abort_role(&mut self) {
        match &mut self.role {
            Role::Download(receiver) => {
                let _ = receiver.abort();
            }
            Role::Upload { sender } => sender.abort(),
        }
    }

    fn on_event(&mut self, event: OwnedEvent) {
        match event {
            OwnedEvent::FileStarted { name, size } => {
                if let Err(err) = self.begin_download(&name) {
                    self.events.push(TransferEvent::Aborted(err));
                    self.done = true;
                    return;
                }
                let path = self
                    .current_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                let size = size.map_or_else(String::new, |size| format!("，{size} 字节"));
                self.events.push(TransferEvent::Info(format!(
                    "正在接收 {name}{size} → {path}"
                )));
            }
            OwnedEvent::FileCompleted => {
                self.file = None;
                if let Some(path) = self.current_path.take() {
                    let verb = if self.is_upload() {
                        "已发送"
                    } else {
                        "已保存到"
                    };
                    self.events
                        .push(TransferEvent::Info(format!("{verb} {}", path.display())));
                }
                // 上传只有一个文件：发完立即请求结束会话（sender 发 ZFIN）。
                if let Role::Upload { sender } = &mut self.role
                    && let Err(err) = sender.finish()
                {
                    self.events
                        .push(TransferEvent::Aborted(format!("结束会话失败：{err}")));
                    self.done = true;
                }
            }
            OwnedEvent::SessionCompleted => {
                self.done = true;
                if let Some(path) = self.current_path.take() {
                    let verb = if self.is_upload() {
                        "已发送"
                    } else {
                        "已保存到"
                    };
                    self.events
                        .push(TransferEvent::Info(format!("{verb} {}", path.display())));
                }
                self.events.push(TransferEvent::Finished);
            }
            OwnedEvent::Aborted => {
                self.done = true;
                self.file = None;
                self.events
                    .push(TransferEvent::Aborted("传输已取消".to_owned()));
            }
        }
    }

    fn begin_download(&mut self, name: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.download_dir).map_err(|err| err.to_string())?;
        let name = sanitize_name(name);
        let path = unique_path(&self.download_dir, &name);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|err| format!("创建 {} 失败：{err}", path.display()))?;
        self.file = Some(file);
        self.current_path = Some(path);
        Ok(())
    }
}

enum Work {
    Wire(Vec<u8>),
    File(Vec<u8>),
    Read(u32, usize),
    Event(OwnedEvent),
    Idle,
}

enum OwnedEvent {
    FileStarted { name: String, size: Option<u32> },
    FileCompleted,
    SessionCompleted,
    Aborted,
}

impl From<Event<'_>> for OwnedEvent {
    fn from(event: Event<'_>) -> Self {
        match event {
            Event::FileStarted(info) => Self::FileStarted {
                name: String::from_utf8_lossy(info.name).into_owned(),
                size: info.size.map(Position::get),
            },
            Event::FileCompleted => Self::FileCompleted,
            Event::SessionCompleted => Self::SessionCompleted,
            Event::Aborted => Self::Aborted,
            // `Event` 是非穷尽枚举：未知事件按中止处理，避免挂着不结束。
            _ => Self::Aborted,
        }
    }
}

/// 下载目录：`$LIETERM_DOWNLOADS` → `$HOME/Downloads` → 当前目录。
pub fn download_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LIETERM_DOWNLOADS") {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join("Downloads");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 只保留文件名部分，并去掉控制字符，避免远端路径穿越。
fn sanitize_name(name: &str) -> String {
    let name = Path::new(name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = name
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\')
        .collect();
    if cleaned.is_empty() {
        "download".to_owned()
    } else {
        cleaned
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned());
    for index in 1..10_000 {
        let name = match &extension {
            Some(extension) => format!("{stem}-{index}.{extension}"),
            None => format!("{stem}-{index}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}", 10_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lieterm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 驱动模拟对端（`Sender`）把文件推给我们的下载 `Transfer`。
    fn pump_sender(sender: &mut Sender, payload: &[u8], incoming: &mut Vec<u8>, out: &mut Vec<u8>) {
        loop {
            let mut consumed = 0;
            while consumed < incoming.len() {
                match sender.submit_wire(&incoming[consumed..]) {
                    Ok(0) => break,
                    Ok(len) => consumed += len,
                    Err(err) => panic!("sender 消费失败：{err}"),
                }
            }
            incoming.drain(..consumed);
            let before = out.len();
            loop {
                match sender.poll() {
                    Action::WriteWire(data) => {
                        let chunk = data.to_vec();
                        out.extend_from_slice(&chunk);
                        sender.wire_written(chunk.len());
                    }
                    Action::ReadFile { offset, max_len } => {
                        let start = offset.get() as usize;
                        let end = (start + max_len).min(payload.len());
                        sender
                            .submit_file(&payload[start..end])
                            .expect("提交文件块");
                    }
                    // 单文件发送完成后必须结束会话，对端才会收到 ZFIN。
                    Action::Event(Event::FileCompleted) => {
                        sender.finish().expect("finish");
                    }
                    Action::Idle => break,
                    _ => break,
                }
            }
            if consumed == 0 && out.len() == before {
                break;
            }
        }
    }

    /// 驱动模拟对端（`Receiver`）接收我们上传 `Transfer` 的字节。
    fn pump_receiver(
        receiver: &mut Receiver,
        incoming: &mut Vec<u8>,
        out: &mut Vec<u8>,
        received: &mut Vec<u8>,
    ) {
        loop {
            let mut consumed = 0;
            while consumed < incoming.len() {
                match receiver.submit_wire(&incoming[consumed..]) {
                    Ok(0) => break,
                    Ok(len) => consumed += len,
                    Err(err) => panic!("receiver 消费失败：{err}"),
                }
            }
            incoming.drain(..consumed);
            let before = out.len() + received.len();
            loop {
                match receiver.poll() {
                    Action::WriteWire(data) => {
                        let chunk = data.to_vec();
                        out.extend_from_slice(&chunk);
                        receiver.wire_written(chunk.len());
                    }
                    Action::WriteFile(data) => {
                        let chunk = data.to_vec();
                        received.extend_from_slice(&chunk);
                        let _ = receiver.file_written(chunk.len());
                    }
                    Action::Idle => break,
                    _ => break,
                }
            }
            if consumed == 0 && out.len() + received.len() == before {
                break;
            }
        }
    }

    #[test]
    fn receives_a_file_end_to_end() {
        let dir = temp_dir("zmodem-download");
        let payload = b"hello zmodem\n".repeat(200);
        let mut sender = Sender::new().expect("sender");
        let mut info = FileInfo::new(b"hello.txt", Some(Position::new(payload.len() as u32)));
        info.mode = Some(0o100_644);
        sender.start_file(info).expect("start_file");

        let mut download = Transfer::download_in(dir.clone()).expect("download");
        let mut to_sender = download.feed(b"");
        let mut to_download = Vec::new();
        for _ in 0..10_000 {
            pump_sender(&mut sender, &payload, &mut to_sender, &mut to_download);
            let back = download.feed(&to_download);
            to_download.clear();
            to_sender.extend(back);
            if download.is_done() {
                break;
            }
        }
        assert!(download.is_done(), "下载应在有限步内结束");
        let saved = std::fs::read(dir.join("hello.txt")).expect("保存的文件");
        assert_eq!(saved, payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sends_a_file_end_to_end() {
        let dir = temp_dir("zmodem-upload");
        let payload = b"upload zmodem\n".repeat(200);
        let source = dir.join("up.bin");
        std::fs::write(&source, &payload).expect("写源文件");

        let mut receiver = Receiver::with_flow_control(0, true).expect("receiver");
        let mut transfer = Transfer::upload().expect("upload");
        let mut from_transfer = transfer.begin_upload(&source).expect("begin_upload");
        let mut from_receiver = Vec::new();
        let mut received = Vec::new();
        for _ in 0..10_000 {
            pump_receiver(
                &mut receiver,
                &mut from_transfer,
                &mut from_receiver,
                &mut received,
            );
            let out = transfer.feed(&from_receiver);
            from_receiver.clear();
            from_transfer.extend(out);
            if transfer.is_done() {
                break;
            }
        }
        assert!(transfer.is_done(), "上传应在有限步内结束");
        assert_eq!(received, payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_download_and_upload_headers() {
        match detect(b"junk**\x18B00\x00\x00\x00\x00\x00\x00") {
            Probe::Start { from, direction } => {
                assert_eq!(from, 4);
                assert_eq!(direction, Direction::Download);
            }
            _ => panic!("应识别为下载"),
        }
        match detect(b"**\x18B01\x00\x00\x00\x00\x00\x00") {
            Probe::Start { direction, .. } => assert_eq!(direction, Direction::Upload),
            _ => panic!("应识别为上传"),
        }
        match detect(b"**\x18A\x00rest") {
            Probe::Start { direction, .. } => assert_eq!(direction, Direction::Download),
            _ => panic!("binary 头应识别为下载"),
        }
    }

    #[test]
    fn partial_header_is_kept_for_next_chunk() {
        assert!(matches!(detect(b"hello**\x18B"), Probe::Partial(5)));
        assert!(matches!(detect(b"hello**\x18B0"), Probe::Partial(5)));
        assert!(matches!(detect(b"hello**\x18"), Probe::Partial(5)));
    }

    #[test]
    fn only_marker_prefixes_stay_buffered() {
        assert_eq!(pending_suffix_len(b"normal output"), 0);
        assert_eq!(pending_suffix_len(b"prompt$ "), 0);
        assert_eq!(pending_suffix_len(b"data*"), 1);
        assert_eq!(pending_suffix_len(b"data**"), 2);
        assert_eq!(pending_suffix_len(b"data**\x18"), 3);
    }

    #[test]
    fn sanitizes_remote_paths() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a.txt"), "a.txt");
        assert_eq!(sanitize_name(""), "download");
    }
}
