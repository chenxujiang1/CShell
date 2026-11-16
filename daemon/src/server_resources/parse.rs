use cshell_domain::{ServerResources, SessionId};

pub(super) fn parse(bytes: &[u8]) -> Option<ServerResources> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.len() > 4096 || !text.is_ascii() {
        return None;
    }
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != 10 || lines[0] != "CSHELL-RESOURCES-1" || lines[9] != "END-CSHELL-RESOURCES" {
        return None;
    }
    let before = cpu(lines[1])?;
    let after = cpu(lines[2])?;
    let delta = after
        .iter()
        .zip(before)
        .map(|(a, b)| a.checked_sub(b))
        .collect::<Option<Vec<_>>>();
    let cpu_basis_points = delta.and_then(|delta| {
        let total = delta.iter().try_fold(0_u64, |sum, v| sum.checked_add(*v))?;
        let idle = delta[3].checked_add(delta[4])?;
        let busy = total.checked_sub(idle)?;
        (total > 0).then(|| ((u128::from(busy) * 10_000) / u128::from(total)) as u16)
    });
    let mut values = std::collections::BTreeMap::new();
    for line in &lines[3..7] {
        let mut parts = line.split_ascii_whitespace();
        let key = parts.next()?;
        let value = parts.next()?.parse::<u64>().ok()?;
        if !matches!(
            key,
            "MemTotal:" | "MemAvailable:" | "SwapTotal:" | "SwapFree:"
        ) || parts.next()? != "kB"
            || parts.next().is_some()
            || values.insert(key, value).is_some()
        {
            return None;
        }
    }
    let mut parts = lines[7].strip_prefix("load ")?.split_ascii_whitespace();
    let load_milli = [
        milli(parts.next()?)?,
        milli(parts.next()?)?,
        milli(parts.next()?)?,
    ];
    let (running, tasks) = parts.next()?.split_once('/')?;
    running.parse::<u64>().ok()?;
    tasks.parse::<u64>().ok()?;
    let _last_pid = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let mut uptime = lines[8].strip_prefix("uptime ")?.split_ascii_whitespace();
    let uptime_seconds = seconds(uptime.next()?)?;
    seconds(uptime.next()?)?;
    if uptime.next().is_some() {
        return None;
    }
    let sample = ServerResources {
        session: SessionId::new(),
        transport_generation: 0,
        sampled_unix_ms: 1,
        cpu_basis_points,
        memory_total_kib: *values.get("MemTotal:")?,
        memory_available_kib: *values.get("MemAvailable:")?,
        swap_total_kib: *values.get("SwapTotal:")?,
        swap_free_kib: *values.get("SwapFree:")?,
        load_milli,
        uptime_seconds,
    };
    sample.validate(sample.session).then_some(sample)
}

fn cpu(line: &str) -> Option<[u64; 8]> {
    let mut parts = line.split_ascii_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }
    let mut result = [0; 8];
    for value in &mut result {
        *value = parts.next()?.parse().ok()?;
    }
    // guest and guest_nice are already included in user/nice: never add twice.
    for value in parts {
        value.parse::<u64>().ok()?;
    }
    Some(result)
}

fn milli(text: &str) -> Option<u32> {
    let (whole, fractional) = text.split_once('.')?;
    if whole.is_empty()
        || fractional.is_empty()
        || fractional.len() > 3
        || !whole
            .bytes()
            .chain(fractional.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    whole.parse::<u32>().ok()?.checked_mul(1000)?.checked_add(
        fractional
            .parse::<u32>()
            .ok()?
            .checked_mul(10_u32.pow(3 - fractional.len() as u32))?,
    )
}

fn seconds(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.')?;
    if whole.is_empty()
        || fraction.is_empty()
        || fraction.len() > 3
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    whole.parse().ok()
}
