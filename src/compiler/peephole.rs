//! Peephole optimization: fuses accumulator sequences into register
//! superinstructions (fewer dispatches; the accumulator still gets the
//! same value). Never fuses across a jump target, a handler boundary or a
//! generator resume point.

use alloc::vec;
use alloc::vec::Vec;

use crate::bytecode::{BinKind, Handler, Op};

fn bin_kind(op: &Op) -> Option<(BinKind, u16)> {
    Some(match *op {
        Op::Add(r) => (BinKind::Add, r),
        Op::Sub(r) => (BinKind::Sub, r),
        Op::Mul(r) => (BinKind::Mul, r),
        Op::Lt(r) => (BinKind::Lt, r),
        Op::Le(r) => (BinKind::Le, r),
        Op::Gt(r) => (BinKind::Gt, r),
        Op::Ge(r) => (BinKind::Ge, r),
        Op::StrictEq(r) => (BinKind::StrictEq, r),
        Op::StrictNe(r) => (BinKind::StrictNe, r),
        Op::BitAnd(r) => (BinKind::BitAnd, r),
        Op::BitOr(r) => (BinKind::BitOr, r),
        Op::BitXor(r) => (BinKind::BitXor, r),
        _ => return None,
    })
}

/// Ops that set the accumulator without reading it (so a value left in
/// it before them is dead).
fn overwrites_acc(op: &Op) -> bool {
    matches!(
        op,
        Op::Ldar(_)
            | Op::LdaInt(_)
            | Op::LdaUndef
            | Op::LdaNull
            | Op::LdaTrue
            | Op::LdaFalse
            | Op::LdaConst(_)
            | Op::LdaGlobal(_)
            | Op::GetNamed(..)
            | Op::Call(..)
            | Op::New(..)
    )
}

fn is_compare(k: BinKind) -> bool {
    matches!(k, BinKind::Lt | BinKind::Le | BinKind::Gt | BinKind::Ge | BinKind::StrictEq | BinKind::StrictNe)
}

fn targets(op: &Op) -> Option<u32> {
    Some(match *op {
        Op::Jmp(t) | Op::JmpTrue(t) | Op::JmpFalse(t) | Op::JmpNullish(t) | Op::JmpNotNullish(t) | Op::JmpUndefined(t) | Op::JmpNotUndefined(t) => t,
        Op::JmpStrictEq(_, t) | Op::JmpIfIntEq(_, _, t) | Op::IteratorStep(_, t) | Op::ForInNext(_, t) => t,
        Op::CmpJmp { target, .. } => target,
        _ => return None,
    })
}

fn retarget(op: &mut Op, map: &[u32]) {
    let m = |t: u32| map[t as usize];
    *op = match *op {
        Op::Jmp(t) => Op::Jmp(m(t)),
        Op::JmpTrue(t) => Op::JmpTrue(m(t)),
        Op::JmpFalse(t) => Op::JmpFalse(m(t)),
        Op::JmpNullish(t) => Op::JmpNullish(m(t)),
        Op::JmpNotNullish(t) => Op::JmpNotNullish(m(t)),
        Op::JmpUndefined(t) => Op::JmpUndefined(m(t)),
        Op::JmpNotUndefined(t) => Op::JmpNotUndefined(m(t)),
        Op::JmpStrictEq(r, t) => Op::JmpStrictEq(r, m(t)),
        Op::JmpIfIntEq(r, i, t) => Op::JmpIfIntEq(r, i, m(t)),
        Op::IteratorStep(r, t) => Op::IteratorStep(r, m(t)),
        Op::ForInNext(r, t) => Op::ForInNext(r, m(t)),
        Op::CmpJmp { kind, lhs, rhs, target, when } => Op::CmpJmp { kind, lhs, rhs, target: m(target), when },
        other => other,
    };
}

/// Optimizes `ops` in place, remapping handlers and the line table.
pub fn optimize(ops: &mut Vec<Op>, handlers: &mut [Handler], lines: &mut Vec<(u32, u32, u32)>) {
    let n = ops.len();
    // Positions that must stay addressable.
    let mut keep = vec![false; n + 1];
    for op in ops.iter() {
        if let Some(t) = targets(op) {
            keep[t as usize] = true;
        }
        if let Op::Yield(_) | Op::YieldRaw(_) | Op::AsyncGenYield(_) | Op::Await(_) | Op::GeneratorStart = op {
            // The resume point is the next op.
        }
    }
    for (i, op) in ops.iter().enumerate() {
        if matches!(op, Op::Yield(_) | Op::YieldRaw(_) | Op::AsyncGenYield(_) | Op::Await(_) | Op::GeneratorStart) {
            keep[i + 1] = true;
        }
    }
    for h in handlers.iter() {
        keep[h.start as usize] = true;
        keep[h.end as usize] = true;
        keep[h.target as usize] = true;
    }
    let mut out: Vec<Op> = Vec::with_capacity(n);
    let mut map = vec![0u32; n + 1];
    let mut i = 0;
    let fusable = |j: usize, len: usize| (1..len).all(|k| j + k < n && !keep[j + k]);
    while i < n {
        map[i] = out.len() as u32;
        // Ldar a; BIN b; Star c  ->  BinRR(c, b, a)
        if fusable(i, 3) {
            if let (Op::Ldar(a), Some((k, b)), Op::Star(c)) = (&ops[i], bin_kind(&ops[i + 1]), &ops[i + 2]) {
                out.push(Op::BinRR { kind: k, dst: *c, lhs: b, rhs: *a });
                map[i + 1] = out.len() as u32 - 1;
                map[i + 2] = out.len() as u32 - 1;
                i += 3;
                continue;
            }
            // Ldar a; Inc/Dec; Star a  ->  IncReg / DecReg
            if let (Op::Ldar(a), Op::Inc | Op::Dec, Op::Star(c)) = (&ops[i], &ops[i + 1], &ops[i + 2]) {
                if a == c {
                    out.push(if matches!(ops[i + 1], Op::Inc) { Op::IncReg(*a) } else { Op::DecReg(*a) });
                    map[i + 1] = out.len() as u32 - 1;
                    map[i + 2] = out.len() as u32 - 1;
                    i += 3;
                    continue;
                }
            }
            // Ldar a; CMP b; JmpFalse/JmpTrue t  ->  CmpJmp
            if let (Op::Ldar(a), Some((k, b)), Op::JmpFalse(t) | Op::JmpTrue(t)) = (&ops[i], bin_kind(&ops[i + 1]), &ops[i + 2]) {
                if is_compare(k) {
                    let when = matches!(ops[i + 2], Op::JmpTrue(_));
                    out.push(Op::CmpJmp { kind: k, lhs: b, rhs: *a, target: *t, when });
                    map[i + 1] = out.len() as u32 - 1;
                    map[i + 2] = out.len() as u32 - 1;
                    i += 3;
                    continue;
                }
            }
        }
        // LdaUninit; Star r; (constant); Star r  ->  (constant); Star r
        // (a `let` initialized with a constant: the TDZ marker is dead).
        if fusable(i, 4) {
            if let (Op::LdaUninit, Op::Star(a), Op::Star(b)) = (&ops[i], &ops[i + 1], &ops[i + 3]) {
                let constant = matches!(
                    ops[i + 2],
                    Op::LdaInt(_) | Op::LdaUndef | Op::LdaNull | Op::LdaTrue | Op::LdaFalse | Op::LdaConst(_)
                );
                if a == b && constant {
                    map[i + 1] = out.len() as u32;
                    map[i + 2] = out.len() as u32;
                    out.push(ops[i + 2]);
                    map[i + 3] = out.len() as u32;
                    out.push(ops[i + 3]);
                    i += 4;
                    continue;
                }
            }
        }
        // Ldar a; Star b; (op that overwrites acc)  ->  Mov a b
        if fusable(i, 2) && i + 2 < n {
            if let (Op::Ldar(a), Op::Star(b)) = (&ops[i], &ops[i + 1]) {
                if overwrites_acc(&ops[i + 2]) {
                    out.push(Op::Mov(*a, *b));
                    map[i + 1] = out.len() as u32 - 1;
                    i += 2;
                    continue;
                }
            }
        }
        out.push(ops[i]);
        i += 1;
    }
    map[n] = out.len() as u32;
    for op in out.iter_mut() {
        retarget(op, &map);
    }
    for h in handlers.iter_mut() {
        h.start = map[h.start as usize];
        h.end = map[h.end as usize];
        h.target = map[h.target as usize];
    }
    for l in lines.iter_mut() {
        l.0 = map[(l.0 as usize).min(n)];
    }
    lines.dedup_by_key(|l| l.0);
    *ops = out;
}
