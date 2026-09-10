use memchr::memchr;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const ANSI_STATE_BYTES: usize = 80;
const MAX_CSI_BYTES: usize = 64;
const MAX_STYLE_SEGMENTS: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JournalColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JournalStyle {
    pub foreground: JournalColor,
    pub background: JournalColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalStyleSpan {
    pub start: u32,
    pub end: u32,
    pub style: JournalStyle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledLogText {
    pub text: String,
    pub style_spans: Vec<JournalStyleSpan>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
enum MachineState {
    #[default]
    Ground = 0,
    Escape = 1,
    Csi = 2,
    Osc = 3,
    OscEscape = 4,
    ControlString = 5,
    ControlStringEscape = 6,
}

impl MachineState {
    fn decode(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Ground,
            1 => Self::Escape,
            2 => Self::Csi,
            3 => Self::Osc,
            4 => Self::OscEscape,
            5 => Self::ControlString,
            6 => Self::ControlStringEscape,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnsiState {
    machine: MachineState,
    csi: [u8; MAX_CSI_BYTES],
    csi_len: u8,
    csi_overflow: bool,
    style: JournalStyle,
}

impl Default for AnsiState {
    fn default() -> Self {
        Self {
            machine: MachineState::Ground,
            csi: [0; MAX_CSI_BYTES],
            csi_len: 0,
            csi_overflow: false,
            style: JournalStyle::default(),
        }
    }
}

impl AnsiState {
    pub(crate) fn advance(&mut self, input: &[u8]) {
        let mut position = 0_usize;
        while position < input.len() {
            if self.machine == MachineState::Ground {
                let Some(escape) = memchr(0x1b, &input[position..]) else {
                    return;
                };
                position = position.saturating_add(escape);
            }
            self.process(&input[position..position + 1], None);
            position += 1;
        }
    }

    pub(crate) fn decode_text(&mut self, input: &[u8], maximum_bytes: usize) -> StyledLogText {
        let mut collector = Collector::new(maximum_bytes);
        if self.machine == MachineState::Ground && input.iter().all(|byte| is_visible_byte(*byte)) {
            collector.push_slice(input, self.style);
        } else {
            let mut position = 0_usize;
            while position < input.len() {
                if self.machine == MachineState::Ground {
                    let visible = input[position..]
                        .iter()
                        .position(|byte| !is_visible_byte(*byte))
                        .unwrap_or(input.len() - position);
                    if visible > 0 {
                        collector.push_slice(&input[position..position + visible], self.style);
                        position += visible;
                        continue;
                    }
                }
                self.process(&input[position..position + 1], Some(&mut collector));
                position += 1;
            }
        }
        collector.finish()
    }

    fn process(&mut self, input: &[u8], mut collector: Option<&mut Collector>) {
        for byte in input {
            match self.machine {
                MachineState::Ground => match *byte {
                    0x1b => self.machine = MachineState::Escape,
                    b'\t' | 0x20..=0x7e | 0x80..=0xff => {
                        if let Some(output) = collector.as_deref_mut() {
                            output.push(*byte, self.style);
                        }
                    }
                    _ => {}
                },
                MachineState::Escape => {
                    self.machine = match *byte {
                        b'[' => {
                            self.csi_len = 0;
                            self.csi_overflow = false;
                            MachineState::Csi
                        }
                        b']' => MachineState::Osc,
                        b'P' | b'X' | b'^' | b'_' => MachineState::ControlString,
                        _ => MachineState::Ground,
                    };
                }
                MachineState::Csi => {
                    if (0x40..=0x7e).contains(byte) {
                        if *byte == b'm' && !self.csi_overflow {
                            let length = usize::from(self.csi_len);
                            let mut parameters = [0_u8; MAX_CSI_BYTES];
                            parameters[..length].copy_from_slice(&self.csi[..length]);
                            self.apply_sgr(&parameters[..length]);
                        }
                        self.machine = MachineState::Ground;
                        self.csi_len = 0;
                        self.csi_overflow = false;
                    } else if (0x20..=0x3f).contains(byte) {
                        let index = usize::from(self.csi_len);
                        if index < self.csi.len() {
                            self.csi[index] = *byte;
                            self.csi_len = self.csi_len.saturating_add(1);
                        } else {
                            self.csi_overflow = true;
                        }
                    }
                }
                MachineState::Osc => match *byte {
                    0x07 => self.machine = MachineState::Ground,
                    0x1b => self.machine = MachineState::OscEscape,
                    _ => {}
                },
                MachineState::OscEscape => {
                    self.machine = if *byte == b'\\' {
                        MachineState::Ground
                    } else {
                        MachineState::Osc
                    };
                }
                MachineState::ControlString => {
                    if *byte == 0x1b {
                        self.machine = MachineState::ControlStringEscape;
                    }
                }
                MachineState::ControlStringEscape => {
                    self.machine = if *byte == b'\\' {
                        MachineState::Ground
                    } else {
                        MachineState::ControlString
                    };
                }
            }
        }
    }

    fn apply_sgr(&mut self, parameters: &[u8]) {
        if parameters.is_empty() {
            self.apply_basic_sgr(0);
            return;
        }
        let mut parameters_by_semicolon = [&[][..]; 32];
        let mut count = 0_usize;
        for parameter in parameters.split(|byte| *byte == b';') {
            if count == parameters_by_semicolon.len() {
                return;
            }
            parameters_by_semicolon[count] = parameter;
            count += 1;
        }
        let mut index = 0_usize;
        while index < count {
            let parameter = parameters_by_semicolon[index];
            if parameter.contains(&b':') {
                self.apply_colon_sgr(parameter);
                index += 1;
                continue;
            }
            let Some(value) = parse_sgr_number(parameter, true) else {
                return;
            };
            if matches!(value, 38 | 48) {
                let foreground = value == 38;
                let mode = parameters_by_semicolon
                    .get(index + 1)
                    .and_then(|parameter| parse_sgr_number(parameter, true));
                if mode == Some(5) && index + 2 < count {
                    let color = parse_sgr_number(parameters_by_semicolon[index + 2], true)
                        .map(|value| JournalColor::Indexed(value.min(255) as u8));
                    if let Some(color) = color {
                        set_color(&mut self.style, foreground, color);
                        index += 3;
                        continue;
                    }
                } else if mode == Some(2) && index + 4 < count {
                    let color = rgb_color(
                        parse_sgr_number(parameters_by_semicolon[index + 2], true),
                        parse_sgr_number(parameters_by_semicolon[index + 3], true),
                        parse_sgr_number(parameters_by_semicolon[index + 4], true),
                    );
                    if let Some(color) = color {
                        set_color(&mut self.style, foreground, color);
                        index += 5;
                        continue;
                    }
                }
            } else {
                self.apply_basic_sgr(value);
            }
            index += 1;
        }
    }

    fn apply_colon_sgr(&mut self, parameter: &[u8]) {
        let mut fields = [None; 8];
        let mut count = 0_usize;
        for field in parameter.split(|byte| *byte == b':') {
            if count == fields.len() {
                return;
            }
            fields[count] = parse_sgr_number(field, false);
            count += 1;
        }
        let Some(code) = fields[0] else {
            return;
        };
        if count == 1 {
            self.apply_basic_sgr(code);
            return;
        }
        if code == 4 {
            self.style.underline = true;
            return;
        }
        if !matches!(code, 38 | 48) {
            return;
        }
        let foreground = code == 38;
        let color = match fields[1] {
            Some(5) if count == 3 => clamp_color(fields[2]).map(JournalColor::Indexed),
            Some(2) if count == 5 => rgb_color(fields[2], fields[3], fields[4]),
            Some(2) if count == 6 && matches!(fields[2], None | Some(0)) => {
                rgb_color(fields[3], fields[4], fields[5])
            }
            _ => None,
        };
        let Some(color) = color else {
            return;
        };
        set_color(&mut self.style, foreground, color);
    }

    fn apply_basic_sgr(&mut self, value: u16) {
        match value {
            0 => self.style = JournalStyle::default(),
            1 => self.style.bold = true,
            3 => self.style.italic = true,
            4 => self.style.underline = true,
            7 => self.style.inverse = true,
            22 => self.style.bold = false,
            23 => self.style.italic = false,
            24 => self.style.underline = false,
            27 => self.style.inverse = false,
            30..=37 => self.style.foreground = JournalColor::Indexed((value - 30) as u8),
            39 => self.style.foreground = JournalColor::Default,
            40..=47 => self.style.background = JournalColor::Indexed((value - 40) as u8),
            49 => self.style.background = JournalColor::Default,
            90..=97 => self.style.foreground = JournalColor::Indexed((value - 90 + 8) as u8),
            100..=107 => self.style.background = JournalColor::Indexed((value - 100 + 8) as u8),
            _ => {}
        }
    }

    pub(crate) fn encode(self, output: &mut Vec<u8>) {
        output.push(self.machine as u8);
        output.push(self.csi_len);
        output.push(u8::from(self.csi_overflow));
        output.push(0);
        output.extend_from_slice(&self.csi);
        encode_style(self.style, output);
        output.extend_from_slice(&[0; 3]);
    }

    pub(crate) fn decode(input: &[u8]) -> Option<Self> {
        if input.len() != ANSI_STATE_BYTES || input[3] != 0 || input[77..] != [0; 3] {
            return None;
        }
        let machine = MachineState::decode(input[0])?;
        let csi_len = input[1];
        if usize::from(csi_len) > MAX_CSI_BYTES || input[2] > 1 {
            return None;
        }
        let mut csi = [0_u8; MAX_CSI_BYTES];
        csi.copy_from_slice(&input[4..68]);
        let style = decode_style(&input[68..77])?;
        Some(Self {
            machine,
            csi,
            csi_len,
            csi_overflow: input[2] != 0,
            style,
        })
    }
}

fn parse_sgr_number(input: &[u8], empty_is_zero: bool) -> Option<u16> {
    if input.is_empty() {
        return empty_is_zero.then_some(0);
    }
    std::str::from_utf8(input).ok()?.parse().ok()
}

fn clamp_color(value: Option<u16>) -> Option<u8> {
    value.map(|value| value.min(255) as u8)
}

fn rgb_color(red: Option<u16>, green: Option<u16>, blue: Option<u16>) -> Option<JournalColor> {
    Some(JournalColor::Rgb(
        clamp_color(red)?,
        clamp_color(green)?,
        clamp_color(blue)?,
    ))
}

fn is_visible_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | 0x20..=0x7e | 0x80..=0xff)
}

fn set_color(style: &mut JournalStyle, foreground: bool, color: JournalColor) {
    if foreground {
        style.foreground = color;
    } else {
        style.background = color;
    }
}

fn encode_style(style: JournalStyle, output: &mut Vec<u8>) {
    encode_color(style.foreground, output);
    encode_color(style.background, output);
    output.push(
        u8::from(style.bold)
            | (u8::from(style.italic) << 1)
            | (u8::from(style.underline) << 2)
            | (u8::from(style.inverse) << 3),
    );
}

fn encode_color(color: JournalColor, output: &mut Vec<u8>) {
    match color {
        JournalColor::Default => output.extend_from_slice(&[0, 0, 0, 0]),
        JournalColor::Indexed(value) => output.extend_from_slice(&[1, value, 0, 0]),
        JournalColor::Rgb(red, green, blue) => output.extend_from_slice(&[2, red, green, blue]),
    }
}

fn decode_style(input: &[u8]) -> Option<JournalStyle> {
    let flags = *input.get(8)?;
    if flags & !0x0f != 0 {
        return None;
    }
    Some(JournalStyle {
        foreground: decode_color(input.get(0..4)?)?,
        background: decode_color(input.get(4..8)?)?,
        bold: flags & 1 != 0,
        italic: flags & 2 != 0,
        underline: flags & 4 != 0,
        inverse: flags & 8 != 0,
    })
}

fn decode_color(input: &[u8]) -> Option<JournalColor> {
    Some(match input {
        [0, 0, 0, 0] => JournalColor::Default,
        [1, value, 0, 0] => JournalColor::Indexed(*value),
        [2, red, green, blue] => JournalColor::Rgb(*red, *green, *blue),
        _ => return None,
    })
}

struct Collector {
    maximum_bytes: usize,
    collected_bytes: usize,
    segments: Vec<(JournalStyle, Vec<u8>)>,
    discarded: bool,
    style_degraded: bool,
}

impl Collector {
    fn new(maximum_bytes: usize) -> Self {
        Self {
            maximum_bytes,
            collected_bytes: 0,
            segments: Vec::new(),
            discarded: false,
            style_degraded: false,
        }
    }

    fn push(&mut self, byte: u8, style: JournalStyle) {
        self.push_slice(std::slice::from_ref(&byte), style);
    }

    fn push_slice(&mut self, bytes_to_add: &[u8], style: JournalStyle) {
        let remaining = self
            .maximum_bytes
            .saturating_add(4)
            .saturating_sub(self.collected_bytes);
        let accepted = bytes_to_add.len().min(remaining);
        if accepted < bytes_to_add.len() {
            self.discarded = true;
        }
        if accepted == 0 {
            return;
        }
        if let Some((_, bytes)) = self
            .segments
            .last_mut()
            .filter(|(current, _)| *current == style)
        {
            bytes.extend_from_slice(&bytes_to_add[..accepted]);
        } else if self.segments.len() >= MAX_STYLE_SEGMENTS {
            if let Some((_, bytes)) = self.segments.last_mut() {
                bytes.extend_from_slice(&bytes_to_add[..accepted]);
            }
            self.style_degraded = true;
        } else {
            self.segments
                .push((style, bytes_to_add[..accepted].to_vec()));
        }
        self.collected_bytes = self.collected_bytes.saturating_add(accepted);
    }

    fn finish(self) -> StyledLogText {
        let mut text = String::new();
        let mut raw_spans = Vec::new();
        for (style, bytes) in self.segments {
            let start = text.len();
            text.push_str(&String::from_utf8_lossy(&bytes));
            let end = text.len();
            if style != JournalStyle::default() && start < end {
                raw_spans.push((start, end, style));
            }
        }
        let mut truncated =
            self.discarded || self.style_degraded || text.len() > self.maximum_bytes;
        if text.len() > self.maximum_bytes {
            let mut boundary = self.maximum_bytes;
            while boundary > 0 && !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text.truncate(boundary);
        }
        if raw_spans.is_empty() {
            return StyledLogText {
                text,
                style_spans: Vec::new(),
                truncated,
            };
        }
        let mut style_spans = Vec::<JournalStyleSpan>::new();
        if text.is_ascii() {
            for (start, end, style) in raw_spans {
                let end = end.min(text.len());
                if start >= end {
                    continue;
                }
                let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) else {
                    truncated = true;
                    break;
                };
                if let Some(previous) = style_spans
                    .last_mut()
                    .filter(|span| span.end == start && span.style == style)
                {
                    previous.end = end;
                } else {
                    style_spans.push(JournalStyleSpan { start, end, style });
                }
            }
            return StyledLogText {
                text,
                style_spans,
                truncated,
            };
        }
        let last_styled_end = raw_spans.last().map_or(0, |(_, end, _)| *end);
        let mut raw_span_index = 0_usize;
        for (start, grapheme) in text.grapheme_indices(true) {
            if start >= last_styled_end {
                break;
            }
            let end = start + grapheme.len();
            while raw_spans
                .get(raw_span_index)
                .is_some_and(|(_, span_end, _)| *span_end <= start)
            {
                raw_span_index += 1;
            }
            let style = raw_spans
                .get(raw_span_index)
                .filter(|(span_start, span_end, _)| *span_start <= start && start < *span_end)
                .map_or_else(JournalStyle::default, |(_, _, style)| *style);
            if style == JournalStyle::default() {
                continue;
            }
            if let Some(previous) = style_spans
                .last_mut()
                .filter(|span| span.end as usize == start && span.style == style)
            {
                previous.end = end as u32;
            } else if let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) {
                style_spans.push(JournalStyleSpan { start, end, style });
            } else {
                truncated = true;
                break;
            }
        }
        StyledLogText {
            text,
            style_spans,
            truncated,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{AnsiState, JournalColor, JournalStyle};

    #[test]
    fn decodes_sgr_true_color_and_sanitizes_control_strings() {
        let mut state = AnsiState::default();
        let output = state.decode_text(
            b"plain \x1b[1;38;2;1;2;3mred\x1b]0;hidden\x07!\x1b[0m end",
            1024,
        );
        assert_eq!(output.text, "plain red! end");
        assert_eq!(output.style_spans.len(), 1);
        assert_eq!(
            &output.text[output.style_spans[0].start as usize..output.style_spans[0].end as usize],
            "red!"
        );
        assert_eq!(
            output.style_spans[0].style,
            JournalStyle {
                foreground: JournalColor::Rgb(1, 2, 3),
                bold: true,
                ..JournalStyle::default()
            }
        );
    }

    #[test]
    fn decodes_colon_sgr_colors_in_sequence_order() {
        let output = AnsiState::default().decode_text(
            b"\x1b[31mred\x1b[38:2::1:2:3mtrue\x1b[48:5:200mbg\x1b[0;38:2:4:5:6mshort\x1b[4:3munder",
            1024,
        );
        assert_eq!(output.text, "redtruebgshortunder");
        assert_eq!(output.style_spans.len(), 5);
        assert_eq!(
            output.style_spans[0].style.foreground,
            JournalColor::Indexed(1)
        );
        assert_eq!(
            output.style_spans[1].style.foreground,
            JournalColor::Rgb(1, 2, 3)
        );
        assert_eq!(
            output.style_spans[2].style.background,
            JournalColor::Indexed(200)
        );
        assert_eq!(
            output.style_spans[3].style.foreground,
            JournalColor::Rgb(4, 5, 6)
        );
        assert!(output.style_spans[4].style.underline);
    }

    #[test]
    fn unsupported_colon_color_space_does_not_change_the_current_color() {
        let output =
            AnsiState::default().decode_text(b"\x1b[32mgreen\x1b[38:2:1:9:8:7mstill-green", 1024);
        assert_eq!(output.text, "greenstill-green");
        assert_eq!(output.style_spans.len(), 1);
        assert_eq!(
            output.style_spans[0].style.foreground,
            JournalColor::Indexed(2)
        );
    }

    #[test]
    fn state_round_trip_preserves_a_split_colon_csi_sequence() {
        let mut state = AnsiState::default();
        state.advance(b"\x1b[38:2::10:");
        let mut encoded = Vec::new();
        state.encode(&mut encoded);
        let mut restored = AnsiState::decode(&encoded).unwrap();
        let output = restored.decode_text(b"20:30mcolored", 64);
        assert_eq!(output.text, "colored");
        assert_eq!(
            output.style_spans[0].style.foreground,
            JournalColor::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn state_round_trip_preserves_a_split_csi_sequence() {
        let mut state = AnsiState::default();
        state.advance(b"\x1b[38;5;");
        let mut encoded = Vec::new();
        state.encode(&mut encoded);
        let mut restored = AnsiState::decode(&encoded).unwrap();
        let output = restored.decode_text(b"196mred", 64);
        assert_eq!(output.text, "red");
        assert_eq!(
            output.style_spans[0].style.foreground,
            JournalColor::Indexed(196)
        );
    }

    #[test]
    fn style_spans_expand_to_grapheme_boundaries() {
        let mut state = AnsiState::default();
        let output = state.decode_text(b"\x1b[31me\x1b[0m\xcc\x81", 64);
        assert_eq!(output.text, "e\u{301}");
        assert_eq!(output.style_spans[0].start, 0);
        assert_eq!(output.style_spans[0].end as usize, output.text.len());
    }

    #[test]
    fn adversarial_style_switches_remain_bounded() {
        let mut input = Vec::new();
        for index in 0..20_000 {
            if index % 2 == 0 {
                input.extend_from_slice(b"\x1b[31mx");
            } else {
                input.extend_from_slice(b"\x1b[0mx");
            }
        }
        let output = AnsiState::default().decode_text(&input, 1_000_000);
        assert_eq!(output.text.len(), 20_000);
        assert!(output.style_spans.len() <= super::MAX_STYLE_SEGMENTS);
        assert!(output.truncated);
    }
}
