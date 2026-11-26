//! Date (ECMA-262 §21.4). Local time comes from the host
//! (`Host::local_tz_offset`, minutes east of UTC); without one, UTC.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::object::arg;
use crate::math::{floor, trunc};
use crate::object::{attr, Class, Key};
use crate::value::Value;
use crate::vm::conv::Hint;
use crate::vm::{JsResult, Vm};

const MS_PER_DAY: f64 = 86_400_000.0;

fn day(t: f64) -> f64 {
    floor(t / MS_PER_DAY)
}

fn time_within_day(t: f64) -> f64 {
    crate::math::rem_euclid(t, MS_PER_DAY)
}

fn days_in_year(y: f64) -> f64 {
    if y % 4.0 != 0.0 {
        365.0
    } else if y % 100.0 != 0.0 {
        366.0
    } else if y % 400.0 != 0.0 {
        365.0
    } else {
        366.0
    }
}

fn day_from_year(y: f64) -> f64 {
    365.0 * (y - 1970.0) + floor((y - 1969.0) / 4.0) - floor((y - 1901.0) / 100.0) + floor((y - 1601.0) / 400.0)
}

fn time_from_year(y: f64) -> f64 {
    MS_PER_DAY * day_from_year(y)
}

fn year_from_time(t: f64) -> f64 {
    let mut y = floor(t / (MS_PER_DAY * 365.2425)) + 1970.0;
    while time_from_year(y) > t {
        y -= 1.0;
    }
    while time_from_year(y + 1.0) <= t {
        y += 1.0;
    }
    y
}

fn in_leap_year(t: f64) -> bool {
    days_in_year(year_from_time(t)) == 366.0
}

fn day_within_year(t: f64) -> f64 {
    day(t) - day_from_year(year_from_time(t))
}

const CUM: [f64; 12] = [0.0, 31.0, 59.0, 90.0, 120.0, 151.0, 181.0, 212.0, 243.0, 273.0, 304.0, 334.0];

fn month_from_time(t: f64) -> f64 {
    let d = day_within_year(t);
    let leap = if in_leap_year(t) { 1.0 } else { 0.0 };
    for m in (0..12).rev() {
        let start = CUM[m] + if m >= 2 { leap } else { 0.0 };
        if d >= start {
            return m as f64;
        }
    }
    0.0
}

fn date_from_time(t: f64) -> f64 {
    let m = month_from_time(t) as usize;
    let leap = if in_leap_year(t) && m >= 2 { 1.0 } else { 0.0 };
    day_within_year(t) - CUM[m] - leap + 1.0
}

fn week_day(t: f64) -> f64 {
    crate::math::rem_euclid(day(t) + 4.0, 7.0)
}

fn hour_from_time(t: f64) -> f64 {
    crate::math::rem_euclid(floor(t / 3_600_000.0), 24.0)
}

fn min_from_time(t: f64) -> f64 {
    crate::math::rem_euclid(floor(t / 60_000.0), 60.0)
}

fn sec_from_time(t: f64) -> f64 {
    crate::math::rem_euclid(floor(t / 1000.0), 60.0)
}

fn ms_from_time(t: f64) -> f64 {
    crate::math::rem_euclid(t, 1000.0)
}

fn to_int(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { trunc(x) }
}

fn make_time(h: f64, m: f64, s: f64, ms: f64) -> f64 {
    if !(h.is_finite() && m.is_finite() && s.is_finite() && ms.is_finite()) {
        return f64::NAN;
    }
    to_int(h) * 3_600_000.0 + to_int(m) * 60_000.0 + to_int(s) * 1000.0 + to_int(ms)
}

fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !(year.is_finite() && month.is_finite() && date.is_finite()) {
        return f64::NAN;
    }
    let (y, m, dt) = (to_int(year), to_int(month), to_int(date));
    let ym = y + floor(m / 12.0);
    if ym.abs() > 400_000.0 {
        return f64::NAN;
    }
    let mn = crate::math::rem_euclid(m, 12.0) as usize;
    let leap = if days_in_year(ym) == 366.0 && mn >= 2 { 1.0 } else { 0.0 };
    let t = day_from_year(ym) + CUM[mn] + leap;
    t + dt - 1.0
}

fn make_date(day: f64, time: f64) -> f64 {
    if !(day.is_finite() && time.is_finite()) {
        return f64::NAN;
    }
    let tv = day * MS_PER_DAY + time;
    if !tv.is_finite() { f64::NAN } else { tv }
}

fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8.64e15 {
        return f64::NAN;
    }
    to_int(t) + 0.0
}

fn tz_offset_ms(vm: &mut Vm, utc: f64) -> f64 {
    if !utc.is_finite() {
        return 0.0;
    }
    match vm.host.as_mut() {
        Some(h) => h.local_tz_offset(utc) * 60_000.0,
        None => 0.0,
    }
}

fn local_time(vm: &mut Vm, t: f64) -> f64 {
    t + tz_offset_ms(vm, t)
}

fn utc(vm: &mut Vm, local: f64) -> f64 {
    if !local.is_finite() {
        return f64::NAN;
    }
    // Two passes for the offset at the right instant (DST transitions).
    let guess = local - tz_offset_ms(vm, local);
    local - tz_offset_ms(vm, guess)
}

fn now(vm: &mut Vm) -> f64 {
    match vm.host.as_mut() {
        Some(h) => floor(h.now()),
        None => 0.0,
    }
}

fn this_time(vm: &mut Vm, this: Value) -> JsResult<f64> {
    if let Some(o) = this.as_object() {
        if let Class::Date(t) = vm.obj(o).class {
            return Ok(t);
        }
    }
    Err(vm.type_error("this is not a Date object."))
}

fn set_this_time(vm: &mut Vm, this: Value, t: f64) -> Value {
    let o = this.as_object().unwrap();
    let t = time_clip(t);
    vm.obj_mut(o).class = Class::Date(t);
    Value::number(t)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn year_str(y: f64) -> String {
    if y >= 0.0 { format!("{:04}", y as i64) } else { format!("-{:06}", (-y) as i64) }
}

fn date_string(t: f64) -> String {
    format!("{} {} {:02} {}", DAYS[week_day(t) as usize], MONTHS[month_from_time(t) as usize], date_from_time(t) as i64, year_str(year_from_time(t)))
}

fn time_string(t: f64) -> String {
    format!("{:02}:{:02}:{:02} GMT", hour_from_time(t) as i64, min_from_time(t) as i64, sec_from_time(t) as i64)
}

fn tz_string(vm: &mut Vm, tv: f64) -> String {
    let off = tz_offset_ms(vm, tv) / 60_000.0;
    let sign = if off >= 0.0 { '+' } else { '-' };
    let a = off.abs() as i64;
    format!("{}{:02}{:02}", sign, a / 60, a % 60)
}

fn to_date_string(vm: &mut Vm, tv: f64) -> String {
    if tv.is_nan() {
        return String::from("Invalid Date");
    }
    let t = local_time(vm, tv);
    let tz = tz_string(vm, tv);
    format!("{} {}{}", date_string(t), time_string(t), tz)
}

fn iso_string(t: f64) -> String {
    let y = year_from_time(t);
    let ys = if (0.0..=9999.0).contains(&y) { format!("{:04}", y as i64) } else if y < 0.0 { format!("-{:06}", (-y) as i64) } else { format!("+{:06}", y as i64) };
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        ys,
        month_from_time(t) as i64 + 1,
        date_from_time(t) as i64,
        hour_from_time(t) as i64,
        min_from_time(t) as i64,
        sec_from_time(t) as i64,
        ms_from_time(t) as i64
    )
}

/// Date.parse: the ISO format, then the toString/toUTCString formats.
fn parse_date(vm: &mut Vm, s: &str) -> f64 {
    if let Some(t) = parse_iso(vm, s) {
        return t;
    }
    parse_fallback(vm, s).unwrap_or(f64::NAN)
}

fn parse_iso(vm: &mut Vm, s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let num = |i: &mut usize, n: usize| -> Option<f64> {
        if *i + n > b.len() {
            return None;
        }
        let part = &b[*i..*i + n];
        if !part.iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        *i += n;
        Some(core::str::from_utf8(part).ok()?.parse::<f64>().ok()?)
    };
    let year = match b.first()? {
        b'+' | b'-' => {
            let neg = b[0] == b'-';
            i = 1;
            let y = num(&mut i, 6)?;
            if neg && y == 0.0 {
                return None;
            }
            if neg { -y } else { y }
        }
        _ => num(&mut i, 4)?,
    };
    let (mut month, mut dayn) = (1.0, 1.0);
    if i < b.len() && b[i] == b'-' {
        i += 1;
        month = num(&mut i, 2)?;
        if i < b.len() && b[i] == b'-' {
            i += 1;
            dayn = num(&mut i, 2)?;
        }
    }
    let (mut h, mut mi, mut sec, mut ms) = (0.0, 0.0, 0.0, 0.0);
    let mut has_time = false;
    let mut offset: Option<f64> = None;
    if i < b.len() && b[i] == b'T' {
        has_time = true;
        i += 1;
        h = num(&mut i, 2)?;
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        mi = num(&mut i, 2)?;
        if b.get(i) == Some(&b':') {
            i += 1;
            sec = num(&mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let st = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == st {
                    return None;
                }
                let frac = core::str::from_utf8(&b[st..i]).ok()?;
                let f: f64 = alloc::format!("0.{frac}").parse().ok()?;
                ms = floor(f * 1000.0);
            }
        }
        match b.get(i) {
            Some(b'Z') => {
                offset = Some(0.0);
                i += 1;
            }
            Some(b'+') | Some(b'-') => {
                let sign = if b[i] == b'-' { -1.0 } else { 1.0 };
                i += 1;
                let oh = num(&mut i, 2)?;
                if b.get(i) != Some(&b':') {
                    return None;
                }
                i += 1;
                let om = num(&mut i, 2)?;
                offset = Some(sign * (oh * 60.0 + om));
            }
            _ => {}
        }
    }
    if i != b.len() {
        return None;
    }
    if !(1.0..=12.0).contains(&month) || dayn < 1.0 || dayn > 31.0 || h > 24.0 || mi > 59.0 || sec > 59.0 {
        return None;
    }
    if h == 24.0 && (mi != 0.0 || sec != 0.0 || ms != 0.0) {
        return None;
    }
    let d = make_day(year, month - 1.0, dayn);
    // Reject days beyond the month (e.g. 2021-02-30).
    if date_from_time(d * MS_PER_DAY) != dayn {
        return None;
    }
    let t = make_date(d, make_time(h, mi, sec, ms));
    let t = match offset {
        Some(o) => t - o * 60_000.0,
        None if has_time => utc(vm, t),
        None => t,
    };
    let c = time_clip(t);
    if c.is_nan() { None } else { Some(c) }
}

/// "Tue Feb 01 2022 00:00:00 GMT+0000" and "Tue, 01 Feb 2022 00:00:00 GMT".
fn parse_fallback(vm: &mut Vm, s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(|c: char| c == ' ' || c == ',').filter(|p| !p.is_empty()).collect();
    if parts.len() < 4 {
        return None;
    }
    let mon_idx = |p: &str| MONTHS.iter().position(|m| p.starts_with(m));
    let (mon, dayn, year, rest) = if let Some(m) = mon_idx(parts[1]) {
        (m, parts[2].parse::<f64>().ok()?, parts[3].parse::<f64>().ok()?, &parts[4..])
    } else if let Some(m) = mon_idx(parts[2]) {
        (m, parts[1].parse::<f64>().ok()?, parts[3].parse::<f64>().ok()?, &parts[4..])
    } else {
        return None;
    };
    let (mut h, mut mi, mut sec) = (0.0, 0.0, 0.0);
    let mut off: Option<f64> = None;
    for p in rest {
        if p.contains(':') {
            let t: Vec<&str> = p.split(':').collect();
            h = t.first()?.parse().ok()?;
            mi = t.get(1)?.parse().ok()?;
            sec = t.get(2).map(|x| x.parse().unwrap_or(0.0)).unwrap_or(0.0);
        } else if let Some(z) = p.strip_prefix("GMT").or_else(|| p.strip_prefix("UTC")) {
            off = Some(if z.len() >= 5 {
                let sign = if z.starts_with('-') { -1.0 } else { 1.0 };
                let hh: f64 = z[1..3].parse().ok()?;
                let mm: f64 = z[3..5].parse().ok()?;
                sign * (hh * 60.0 + mm)
            } else {
                0.0
            });
        }
    }
    let t = make_date(make_day(year, mon as f64, dayn), make_time(h, mi, sec, 0.0));
    let t = match off {
        Some(o) => t - o * 60_000.0,
        None => utc(vm, t),
    };
    let c = time_clip(t);
    if c.is_nan() { None } else { Some(c) }
}

fn date_ctor(vm: &mut Vm, _: Value, a: &[Value], nt: Value) -> JsResult<Value> {
    if nt.is_undefined() {
        let t = now(vm);
        let s = to_date_string(vm, t);
        return Ok(vm.new_string(&s));
    }
    let tv = match a.len() {
        0 => now(vm),
        1 => {
            let v = a[0];
            let tv = if let Some(Class::Date(t)) = v.as_object().map(|o| &vm.obj(o).class) {
                *t
            } else {
                let p = vm.to_primitive(v, Hint::Default)?;
                if let Some(s) = p.as_string() {
                    let st = vm.str(s).to_rust_lossy();
                    parse_date(vm, &st)
                } else {
                    vm.to_number(p)?
                }
            };
            time_clip(tv)
        }
        _ => {
            let mut nums = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
            for (i, &v) in a.iter().take(7).enumerate() {
                nums[i] = vm.to_number(v)?;
            }
            let mut y = nums[0];
            if !y.is_nan() {
                let yi = to_int(y);
                if (0.0..=99.0).contains(&yi) {
                    y = 1900.0 + yi;
                }
            }
            let fd = make_date(make_day(y, nums[1], nums[2]), make_time(nums[3], nums[4], nums[5], nums[6]));
            time_clip(utc(vm, fd))
        }
    };
    let p = vm.proto_from_ctor(nt, |i| i.date_prototype)?;
    Ok(Value::object(vm.alloc_object(Some(p), Class::Date(tv))))
}

type Getter = fn(f64) -> f64;

pub fn init(vm: &mut Vm) {
    let op = vm.intrinsics.object_prototype;
    let dp = vm.alloc_object(Some(op), Class::Ordinary);
    vm.intrinsics.date_prototype = dp;
    let c = vm.constructor("Date", 7, date_ctor, dp);
    vm.global_prop("Date", Value::object(c));
    vm.method(c, "now", 0, |vm, _, _, _| Ok(Value::number(now(vm))));
    vm.method(c, "parse", 1, |vm, _, a, _| {
        let s = vm.to_string(arg(a, 0))?;
        let st = vm.str(s).to_rust_lossy();
        Ok(Value::number(parse_date(vm, &st)))
    });
    vm.method(c, "UTC", 7, |vm, _, a, _| {
        let mut nums = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        for (i, &v) in a.iter().take(7).enumerate() {
            nums[i] = vm.to_number(v)?;
        }
        let mut y = nums[0];
        if !y.is_nan() {
            let yi = to_int(y);
            if (0.0..=99.0).contains(&yi) {
                y = 1900.0 + yi;
            }
        }
        Ok(Value::number(time_clip(make_date(make_day(y, nums[1], nums[2]), make_time(nums[3], nums[4], nums[5], nums[6])))))
    });
    // Getters: (name, local?, getter)
    let getters: [(&str, bool, Getter); 16] = [
        ("getDate", true, date_from_time),
        ("getDay", true, week_day),
        ("getFullYear", true, year_from_time),
        ("getHours", true, hour_from_time),
        ("getMilliseconds", true, ms_from_time),
        ("getMinutes", true, min_from_time),
        ("getMonth", true, month_from_time),
        ("getSeconds", true, sec_from_time),
        ("getUTCDate", false, date_from_time),
        ("getUTCDay", false, week_day),
        ("getUTCFullYear", false, year_from_time),
        ("getUTCHours", false, hour_from_time),
        ("getUTCMilliseconds", false, ms_from_time),
        ("getUTCMinutes", false, min_from_time),
        ("getUTCMonth", false, month_from_time),
        ("getUTCSeconds", false, sec_from_time),
    ];
    for (i, (name, _, _)) in getters.iter().enumerate() {
        let f = vm.native_fn_data(name, 0, |vm, this, _, _| {
            let i = vm.native_data(0).as_int().unwrap() as usize;
            let t = this_time(vm, this)?;
            if t.is_nan() {
                return Ok(Value::NAN);
            }
            let table: [(bool, Getter); 16] = [
                (true, date_from_time),
                (true, week_day),
                (true, year_from_time),
                (true, hour_from_time),
                (true, ms_from_time),
                (true, min_from_time),
                (true, month_from_time),
                (true, sec_from_time),
                (false, date_from_time),
                (false, week_day),
                (false, year_from_time),
                (false, hour_from_time),
                (false, ms_from_time),
                (false, min_from_time),
                (false, month_from_time),
                (false, sec_from_time),
            ];
            let (local, g) = table[i];
            let t = if local { local_time(vm, t) } else { t };
            Ok(Value::number(g(t)))
        }, false, alloc::boxed::Box::new([Value::int(i as i32)]));
        let k = vm.key(name);
        vm.define_data(dp, k, Value::object(f), attr::HIDDEN);
    }
    vm.method(dp, "getTime", 0, |vm, this, _, _| Ok(Value::number(this_time(vm, this)?)));
    vm.method(dp, "valueOf", 0, |vm, this, _, _| Ok(Value::number(this_time(vm, this)?)));
    vm.method(dp, "getTimezoneOffset", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        if t.is_nan() {
            return Ok(Value::NAN);
        }
        Ok(Value::number((t - local_time(vm, t)) / 60_000.0))
    });
    vm.method(dp, "getYear", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        if t.is_nan() {
            return Ok(Value::NAN);
        }
        let lt = local_time(vm, t);
        Ok(Value::number(year_from_time(lt) - 1900.0))
    });
    vm.method(dp, "setTime", 1, |vm, this, a, _| {
        this_time(vm, this)?;
        let t = vm.to_number(arg(a, 0))?;
        Ok(set_this_time(vm, this, t))
    });
    // Setters: (name, local, first component index, max args)
    // components: 0 year, 1 month, 2 date, 3 hour, 4 min, 5 sec, 6 ms
    let setters: [(&str, bool, u8, u8); 14] = [
        ("setMilliseconds", true, 6, 1),
        ("setUTCMilliseconds", false, 6, 1),
        ("setSeconds", true, 5, 2),
        ("setUTCSeconds", false, 5, 2),
        ("setMinutes", true, 4, 3),
        ("setUTCMinutes", false, 4, 3),
        ("setHours", true, 3, 4),
        ("setUTCHours", false, 3, 4),
        ("setDate", true, 2, 1),
        ("setUTCDate", false, 2, 1),
        ("setMonth", true, 1, 2),
        ("setUTCMonth", false, 1, 2),
        ("setFullYear", true, 0, 3),
        ("setUTCFullYear", false, 0, 3),
    ];
    for (name, local, first, n) in setters {
        let f = vm.native_fn_data(name, n as u32, date_setter, false, alloc::boxed::Box::new([Value::bool(local), Value::int(first as i32), Value::int(n as i32)]));
        let k = vm.key(name);
        vm.define_data(dp, k, Value::object(f), attr::HIDDEN);
    }
    vm.method(dp, "setYear", 1, |vm, this, a, _| {
        let t = this_time(vm, this)?;
        let y = vm.to_number(arg(a, 0))?;
        if y.is_nan() {
            return Ok(set_this_time(vm, this, f64::NAN));
        }
        let yi = to_int(y);
        let yyyy = if (0.0..=99.0).contains(&yi) { 1900.0 + yi } else { y };
        let lt = if t.is_nan() { 0.0 } else { local_time(vm, t) };
        let d = make_day(yyyy, month_from_time(lt), date_from_time(lt));
        let u = utc(vm, make_date(d, time_within_day(lt)));
        Ok(set_this_time(vm, this, u))
    });
    vm.method(dp, "toISOString", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        if !t.is_finite() {
            return Err(vm.range_error("Invalid time value"));
        }
        Ok(vm.new_string(&iso_string(t)))
    });
    vm.method(dp, "toJSON", 1, |vm, this, _, _| {
        let o = vm.to_object(this)?;
        let tv = vm.to_primitive(Value::object(o), Hint::Number)?;
        if let Some(n) = tv.as_number() {
            if !n.is_finite() {
                return Ok(Value::NULL);
            }
        }
        let k = vm.key("toISOString");
        let f = vm.get(o, k)?;
        if !vm.is_callable(f) {
            return Err(vm.type_error("toISOString is not a function"));
        }
        vm.call(f, Value::object(o), &[])
    });
    vm.method(dp, "toString", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        let s = to_date_string(vm, t);
        Ok(vm.new_string(&s))
    });
    vm.method(dp, "toDateString", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        if t.is_nan() {
            return Ok(vm.new_string("Invalid Date"));
        }
        let lt = local_time(vm, t);
        Ok(vm.new_string(&date_string(lt)))
    });
    vm.method(dp, "toTimeString", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        if t.is_nan() {
            return Ok(vm.new_string("Invalid Date"));
        }
        let lt = local_time(vm, t);
        let tz = tz_string(vm, t);
        Ok(vm.new_string(&format!("{}{}", time_string(lt), tz)))
    });
    let utc_fn = vm.method(dp, "toUTCString", 0, |vm, this, _, _| {
        let t = this_time(vm, this)?;
        if t.is_nan() {
            return Ok(vm.new_string("Invalid Date"));
        }
        let s = format!("{}, {:02} {} {} {}", DAYS[week_day(t) as usize], date_from_time(t) as i64, MONTHS[month_from_time(t) as usize], year_str(year_from_time(t)), time_string(t));
        Ok(vm.new_string(&s))
    });
    let kg = vm.key("toGMTString");
    vm.define_data(dp, kg, Value::object(utc_fn), attr::HIDDEN);
    for name in ["toLocaleString", "toLocaleDateString", "toLocaleTimeString"] {
        let f = vm.native_fn_data(name, 0, |vm, this, _, _| {
            let t = this_time(vm, this)?;
            let s = to_date_string(vm, t);
            Ok(vm.new_string(&s))
        }, false, alloc::boxed::Box::new([]));
        let k = vm.key(name);
        vm.define_data(dp, k, Value::object(f), attr::HIDDEN);
    }
    let tp = vm.intrinsics.sym_to_primitive;
    let f = vm.native_fn("[Symbol.toPrimitive]", 1, |vm, this, a, _| {
        let Some(o) = this.as_object() else {
            return Err(vm.type_error("Date.prototype[Symbol.toPrimitive] called on non-object"));
        };
        let h = arg(a, 0);
        let hs = h.as_string().map(|s| vm.str(s).to_rust_lossy());
        let hint = match hs.as_deref() {
            Some("string") | Some("default") => Hint::String,
            Some("number") => Hint::Number,
            _ => return Err(vm.type_error("Invalid hint")),
        };
        vm.ordinary_to_primitive(o, hint)
    });
    vm.define_data(dp, Key::Sym(tp), Value::object(f), attr::CONFIGURABLE);
}

/// The setXxx / setUTCXxx methods.
fn date_setter(vm: &mut Vm, this: Value, a: &[Value], _: Value) -> JsResult<Value> {
    let local = vm.native_data(0) == Value::TRUE;
    let first = vm.native_data(1).as_int().unwrap() as usize;
    let n = vm.native_data(2).as_int().unwrap() as usize;
    let t = this_time(vm, this)?;
    // Coerce the given arguments (only as many as passed, up to n).
    let mut vals: Vec<f64> = Vec::new();
    for i in 0..n.min(a.len().max(1)) {
        vals.push(vm.to_number(arg(a, i))?);
    }
    let base = if t.is_nan() {
        if first == 0 { 0.0 } else { return Ok(Value::NAN) }
    } else if local {
        local_time(vm, t)
    } else {
        t
    };
    let mut comp = [year_from_time(base), month_from_time(base), date_from_time(base), hour_from_time(base), min_from_time(base), sec_from_time(base), ms_from_time(base)];
    for (i, v) in vals.iter().enumerate() {
        comp[first + i] = *v;
    }
    let nt = make_date(make_day(comp[0], comp[1], comp[2]), make_time(comp[3], comp[4], comp[5], comp[6]));
    let u = if local { utc(vm, nt) } else { nt };
    Ok(set_this_time(vm, this, u))
}
