use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::Rng;
use rand::rngs::OsRng;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x00000010;

#[cfg(windows)]
unsafe extern "system" {
    fn SetConsoleTitleW(lpConsoleTitle: *const u16) -> i32;
    fn SetConsoleCtrlHandler(
        handler_routine: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(windows)]
unsafe extern "system" fn ctrl_c_handler(ctrl_type: u32) -> i32 {
    if ctrl_type == 0 {
        cancel_all_threads();
        return 1;
    }
    0
}

fn set_console_title(title: &str) {
    #[cfg(windows)]
    unsafe {
        let mut wide: Vec<u16> = title.encode_utf16().collect();
        wide.push(0);
        SetConsoleTitleW(wide.as_ptr());
    }
    let _ = title;
}

fn install_ctrl_c_handler() {
    #[cfg(windows)]
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_c_handler), 1);
    }
}

// ============================================================================
// CORE DATA STRUCTURES
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
}

impl Value {
    fn as_f64(&self) -> Result<f64, String> {
        match self {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Str(s) => s.parse::<f64>().map_err(|_| format!("Cannot convert '{}' to number", s)),
            Value::Null => Ok(0.0),
        }
    }

    fn as_i64(&self) -> Result<i64, String> {
        match self {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => Ok(*f as i64),
            Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
            Value::Str(s) => s.parse::<i64>().map_err(|_| format!("Cannot convert '{}' to integer", s)),
            Value::Null => Ok(0),
        }
    }

    fn display_str(&self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                if f.fract() == 0.0 && f.abs() < 1e15 {
                    format!("{:.1}", f)
                } else {
                    f.to_string()
                }
            }
            Value::Str(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum VarTarget {
    GlobalNum(usize),
    GlobalNamed(String),
    LocalNum(usize),
    LocalNamed(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum FuncTarget {
    GlobalNum(usize),
    GlobalNamed(String),
    LocalNum(usize),
    LocalNamed(String),
}

#[derive(Debug, Clone)]
enum FuncDef {
    LegacyPipeline(Vec<(String, Expr)>),
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone)]
enum Expr {
    Lit(Value),
    Var(VarTarget),
    Input,
    SysCall(Box<Expr>),
    TimeElapsed,
    TimeUnix,
    DictGet { idx: Box<Expr>, dict: usize },
    DictFind { val: Box<Expr>, dict: usize },
    Pemdas(String),
    Trig { kind: char, arg: Box<Expr> },
    Round(Box<Expr>),
    FuncCall { target: FuncTarget, arg: Option<Box<Expr>> },
    BinOp { left: Box<Expr>, op: String, right: Box<Expr> },
}

#[derive(Debug, Clone, PartialEq)]
enum CmpOp {
    Eq,
    NotEq,
    Gt,
    Lt,
    Ge,
    Le,
}

#[derive(Debug, Clone)]
enum Stmt {
    Assign { target: VarTarget, expr: Expr },
    AddConstInt { idx: usize, delta: i64 },
    Print(Expr),
    Sleep(Expr),
    Import(Expr),
    SysExec(Expr),
    Rand { target: VarTarget, min: Expr, max: Expr, secure: bool },
    DictAppend { val: Expr, dict: usize },
    DictClear(usize),
    SaveBin { dict: usize, name: Expr },
    LoadBin { dict: usize, name: Expr },
    AngleMode(String),
    FuncDef { target: FuncTarget, def: FuncDef },
    FuncExec { target: FuncTarget, arg: Option<Expr> },
    IfChain { branches: Vec<(Expr, CmpOp, Expr, Vec<Stmt>)>, else_body: Option<Vec<Stmt>> },
    WhileLoop { left: Expr, op: CmpOp, right: Expr, body: Vec<Stmt> },
    IntLoop { idx: usize, op: CmpOp, limit: i64, body: Vec<Stmt> },
    CountLoop { count: Expr, body: Vec<Stmt> },
    TryCatch { try_body: Vec<Stmt>, catch_body: Vec<Stmt> },
    SpawnThread { id: Expr, body: Vec<Stmt> },
    EndThread(Expr),
    TailThread(Expr),
}

// ============================================================================
// EXECUTION STATE & CONCURRENCY
// ============================================================================

struct GlobalState {
    v: Vec<Value>,
    v_named: HashMap<String, Value>,
    d: Vec<Vec<Value>>,
    f: HashMap<usize, FuncDef>,
    f_named: HashMap<String, FuncDef>,
    angle_radians: bool,
    start_time: Instant,
    imported_files: HashSet<PathBuf>,
    base_dir: PathBuf,
}

impl GlobalState {
    fn new(base_dir: PathBuf) -> Self {
        Self {
            v: vec![Value::Null; 256],
            v_named: HashMap::new(),
            d: vec![Vec::new(); 64],
            f: HashMap::new(),
            f_named: HashMap::new(),
            angle_radians: false,
            start_time: Instant::now(),
            imported_files: HashSet::new(),
            base_dir,
        }
    }
}

struct LocalEnv {
    vl: Vec<Value>,
    vl_named: HashMap<String, Value>,
    fl: HashMap<usize, FuncDef>,
    fl_named: HashMap<String, FuncDef>,
    cancel_flag: Option<Arc<AtomicBool>>,
}

impl LocalEnv {
    fn new(cancel_flag: Option<Arc<AtomicBool>>) -> Self {
        Self {
            vl: vec![Value::Null; 64],
            vl_named: HashMap::new(),
            fl: HashMap::new(),
            fl_named: HashMap::new(),
            cancel_flag,
        }
    }
}

thread_local! {
    static LOCAL_ENV: RefCell<LocalEnv> = RefCell::new(LocalEnv::new(None));
}

struct ThreadRegistry {
    handles: Vec<JoinHandle<()>>,
    cancel_flags: HashMap<String, Arc<AtomicBool>>,
}

static THREAD_REG: OnceLock<Mutex<ThreadRegistry>> = OnceLock::new();

fn thread_reg() -> &'static Mutex<ThreadRegistry> {
    THREAD_REG.get_or_init(|| {
        Mutex::new(ThreadRegistry {
            handles: Vec::new(),
            cancel_flags: HashMap::new(),
        })
    })
}

fn cancel_all_threads() {
    if let Ok(reg) = thread_reg().lock() {
        for flag in reg.cancel_flags.values() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

fn join_background_threads() {
    loop {
        let handles: Vec<JoinHandle<()>> = {
            let mut reg = thread_reg().lock().unwrap();
            std::mem::take(&mut reg.handles)
        };
        if handles.is_empty() {
            break;
        }
        for h in handles {
            let _ = h.join();
        }
    }
}

// ============================================================================
// PREPROCESSING & BALANCED EXTRACTION HELPERS
// ============================================================================

fn strip_comments_and_whitespace(src: &str, preserve_newlines: bool) -> String {
    let mut out = String::with_capacity(src.len());
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_string = false;
    let mut in_comment = false;

    while i < chars.len() {
        let c = chars[i];
        if in_comment {
            if c == '?' {
                in_comment = false;
            }
            i += 1;
            continue;
        }
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '?' {
            in_comment = true;
            i += 1;
            continue;
        }
        if c == '\r' || c == '\n' {
            if preserve_newlines {
                out.push('\n');
            }
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn condense_source(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_string = false;
    let mut in_comment = false;

    while i < chars.len() {
        let c = chars[i];
        if in_comment {
            if c == '?' {
                in_comment = false;
            }
            i += 1;
            continue;
        }
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '?' {
            in_comment = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() || c == ';' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn find_matching_delim(s: &str, open_idx: usize, open_ch: char, close_ch: char) -> Result<usize, String> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut i = open_idx;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == open_ch as u8 {
            depth += 1;
        } else if b == close_ch as u8 {
            depth -= 1;
            if depth == 0 {
                return Ok(i);
            }
        }
        i += 1;
    }
    Err(format!("Unclosed delimiter '{}' in: {}", open_ch, &s[open_idx..s.len().min(open_idx + 40)]))
}

fn find_char_outside_quotes(s: &str, target: char) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == target as u8 {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ============================================================================
// LEXER / PARSER (100% WHITESPACE-AGNOSTIC)
// ============================================================================

const ATOM_PAT: &str = r#"(?:Vl\s*"(?:[^"\\]|\\.)*"|V\s*"(?:[^"\\]|\\.)*"|Vl\s*\d+|V\s*\d+|Fl\s*"(?:[^"\\]|\\.)*"\s*\([^)]*\)|F\s*"(?:[^"\\]|\\.)*"\s*\([^)]*\)|Fl\s*\d+\s*\([^)]*\)|F\s*\d+\s*\([^)]*\)|G\s*\d+\s*\(\s*D\s*\d+\s*\)|W\s*\([^)]+,\s*D\s*\d+\s*\)|Sy\s*\([^)]+\)|M[sctar]\s*\([^)]+\)|Tiu|Ti|I|"(?:[^"\\]|\\.)*"|true|false|-?\d+(?:\.\d+)?)"#;

fn atom_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(ATOM_PAT).unwrap())
}

fn cond_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let side = format!(r#"{}(?:\s*(?:[+\-*/%^]|M)\s*{})*"#, ATOM_PAT, ATOM_PAT);
        let pat = format!(r#"^({})\s*(E!=|E>=|E<=|E=>|E=<|E>|E<|E)\s*({})$"#, side, side);
        Regex::new(&pat).unwrap()
    })
}

fn unescape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_var_target(s: &str) -> Result<VarTarget, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("Vl") {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('"') {
            let name = inner.strip_suffix('"').ok_or_else(|| format!("Invalid variable: {}", s))?;
            Ok(VarTarget::LocalNamed(unescape_str(name)))
        } else {
            let idx = rest.parse::<usize>().map_err(|_| format!("Invalid variable: {}", s))?;
            Ok(VarTarget::LocalNum(idx))
        }
    } else if let Some(rest) = s.strip_prefix('V') {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('"') {
            let name = inner.strip_suffix('"').ok_or_else(|| format!("Invalid variable: {}", s))?;
            Ok(VarTarget::GlobalNamed(unescape_str(name)))
        } else {
            let idx = rest.parse::<usize>().map_err(|_| format!("Invalid variable: {}", s))?;
            Ok(VarTarget::GlobalNum(idx))
        }
    } else {
        Err(format!("Invalid variable: {}", s))
    }
}

fn parse_func_target(s: &str) -> Result<FuncTarget, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("Fl") {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('"') {
            let name = inner.strip_suffix('"').ok_or_else(|| format!("Invalid function: {}", s))?;
            Ok(FuncTarget::LocalNamed(unescape_str(name)))
        } else {
            let idx = rest.parse::<usize>().map_err(|_| format!("Invalid function: {}", s))?;
            Ok(FuncTarget::LocalNum(idx))
        }
    } else if let Some(rest) = s.strip_prefix('F') {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('"') {
            let name = inner.strip_suffix('"').ok_or_else(|| format!("Invalid function: {}", s))?;
            Ok(FuncTarget::GlobalNamed(unescape_str(name)))
        } else {
            let idx = rest.parse::<usize>().map_err(|_| format!("Invalid function: {}", s))?;
            Ok(FuncTarget::GlobalNum(idx))
        }
    } else {
        Err(format!("Invalid function: {}", s))
    }
}

fn parse_atom(tok: &str) -> Result<Expr, String> {
    let tok = tok.trim();
    if tok == "I" {
        return Ok(Expr::Input);
    }
    if tok == "Tiu" {
        return Ok(Expr::TimeUnix);
    }
    if tok == "Ti" {
        return Ok(Expr::TimeElapsed);
    }
    if tok == "true" {
        return Ok(Expr::Lit(Value::Bool(true)));
    }
    if tok == "false" {
        return Ok(Expr::Lit(Value::Bool(false)));
    }
    if tok.starts_with('"') && tok.ends_with('"') && tok.len() >= 2 {
        return Ok(Expr::Lit(Value::Str(unescape_str(&tok[1..tok.len() - 1]))));
    }
    if let Some(after_mf) = tok.strip_prefix("Mf") {
        let trimmed = after_mf.trim_start();
        if trimmed.starts_with('(') {
            let open_idx = tok.len() - trimmed.len();
            let end = find_matching_delim(tok, open_idx, '(', ')')?;
            if end == tok.len() - 1 {
                return Ok(Expr::Pemdas(tok[open_idx + 1..end].to_string()));
            }
        }
    }
    if let Some(after_sy) = tok.strip_prefix("Sy") {
        let trimmed = after_sy.trim_start();
        if trimmed.starts_with('(') {
            let open_idx = tok.len() - trimmed.len();
            let end = find_matching_delim(tok, open_idx, '(', ')')?;
            if end == tok.len() - 1 {
                let sub = parse_expr(&tok[open_idx + 1..end])?;
                return Ok(Expr::SysCall(Box::new(sub)));
            }
        }
    }
    if tok.len() > 3 && tok.starts_with('M') {
        let kind = tok.chars().nth(1).unwrap();
        let after_kind = tok[2..].trim_start();
        if after_kind.starts_with('(') {
            let open_idx = tok.len() - after_kind.len();
            let end = find_matching_delim(tok, open_idx, '(', ')')?;
            if end == tok.len() - 1 {
                let inner = &tok[open_idx + 1..end];
                let sub = parse_expr(inner)?;
                if kind == 'r' {
                    return Ok(Expr::Round(Box::new(sub)));
                }
                if matches!(kind, 's' | 'c' | 't' | 'a') {
                    return Ok(Expr::Trig { kind, arg: Box::new(sub) });
                }
            }
        }
    }
    if let Some(rest) = tok.strip_prefix('G') {
        if let Some(open) = rest.find('(') {
            let close = find_matching_delim(rest, open, '(', ')')?;
            if close == rest.len() - 1 {
                let idx_str = &rest[..open];
                let dict_part = rest[open + 1..close].trim();
                if let Some(dict_digits) = dict_part.strip_prefix('D') {
                    let idx_expr = parse_expr(idx_str)?;
                    let dict_idx = dict_digits
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| format!("Invalid dict: {}", dict_part))?;
                    return Ok(Expr::DictGet { idx: Box::new(idx_expr), dict: dict_idx });
                }
            }
        }
    }
    if let Some(after_w) = tok.strip_prefix('W') {
        let trimmed = after_w.trim_start();
        if trimmed.starts_with('(') {
            let open_idx = tok.len() - trimmed.len();
            let end = find_matching_delim(tok, open_idx, '(', ')')?;
            if end == tok.len() - 1 {
                let inner = &tok[open_idx + 1..end];
                if let Some(comma_pos) = inner.rfind(',') {
                    let val_str = inner[..comma_pos].trim();
                    let dict_part = inner[comma_pos + 1..].trim();
                    if let Some(dict_digits) = dict_part.strip_prefix('D') {
                        let val_expr = parse_expr(val_str)?;
                        let dict_idx = dict_digits
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| format!("Invalid dict: {}", dict_part))?;
                        return Ok(Expr::DictFind { val: Box::new(val_expr), dict: dict_idx });
                    }
                }
            }
        }
    }
    if (tok.starts_with("Fl") || tok.starts_with('F')) && tok.ends_with(')') {
        // Find '(' outside quotes
        let mut in_str = false;
        let mut open_pos = None;
        for (idx, b) in tok.as_bytes().iter().enumerate() {
            if in_str {
                if *b == b'"' {
                    in_str = false;
                }
                continue;
            }
            if *b == b'"' {
                in_str = true;
            } else if *b == b'(' {
                open_pos = Some(idx);
                break;
            }
        }
        if let Some(open) = open_pos {
            let target = parse_func_target(&tok[..open])?;
            let arg_str = tok[open + 1..tok.len() - 1].trim();
            let arg = if arg_str.is_empty() { None } else { Some(Box::new(parse_expr(arg_str)?)) };
            return Ok(Expr::FuncCall { target, arg });
        }
    }
    if tok.starts_with("Vl") || tok.starts_with('V') {
        return Ok(Expr::Var(parse_var_target(tok)?));
    }
    if let Ok(i) = tok.parse::<i64>() {
        return Ok(Expr::Lit(Value::Int(i)));
    }
    if let Ok(f) = tok.parse::<f64>() {
        return Ok(Expr::Lit(Value::Float(f)));
    }
    Err(format!("Invalid value: {}", tok))
}

fn split_top_level_expr(s: &str) -> Result<Vec<String>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Empty expression".to_string());
    }
    let bytes = s.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let atom_start = i;
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            let rest = &s[i..];
            let next_op = rest.find(|c: char| "+-*/%^".contains(c)).unwrap_or(usize::MAX);
            let open_paren = rest.find('(').unwrap_or(usize::MAX);

            let has_balanced_call = open_paren < next_op
                && (rest.starts_with("Mf")
                    || rest.starts_with("Sy")
                    || rest.starts_with('W')
                    || (rest.len() > 2 && rest.starts_with('M') && matches!(rest.as_bytes()[1], b's' | b'c' | b't' | b'a' | b'r'))
                    || rest.starts_with("Fl")
                    || rest.starts_with('F')
                    || rest.starts_with('G'));

            if has_balanced_call {
                let close_abs = find_matching_delim(s, i + open_paren, '(', ')')?;
                i = close_abs + 1;
            } else if let Some(m) = atom_regex().find(rest) {
                if m.start() == 0 {
                    i += m.end();
                } else {
                    return Err(format!("Cannot parse expression near: {}", rest));
                }
            } else {
                return Err(format!("Cannot parse expression near: {}", rest));
            }
        }
        tokens.push(s[atom_start..i].to_string());

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let op_ch = bytes[i] as char;
        if matches!(op_ch, '+' | '-' | '*' | '/' | '%' | '^' | 'M') {
            tokens.push(op_ch.to_string());
            i += 1;
        } else {
            return Err(format!("Expected operator in expression near: {}", &s[i..]));
        }
    }

    if tokens.len() % 2 == 0 {
        return Err(format!("Trailing operator in expression: {}", s));
    }
    Ok(tokens)
}

fn parse_expr(s: &str) -> Result<Expr, String> {
    let tokens = split_top_level_expr(s)?;
    let mut expr = parse_atom(&tokens[0])?;
    let mut idx = 1;
    while idx + 1 < tokens.len() {
        let op = tokens[idx].clone();
        let right = parse_atom(&tokens[idx + 1])?;
        expr = Expr::BinOp {
            left: Box::new(expr),
            op,
            right: Box::new(right),
        };
        idx += 2;
    }
    Ok(expr)
}

fn parse_cmp_op(op: &str) -> Result<CmpOp, String> {
    match op {
        "E" => Ok(CmpOp::Eq),
        "E!=" => Ok(CmpOp::NotEq),
        "E>" => Ok(CmpOp::Gt),
        "E<" => Ok(CmpOp::Lt),
        "E>=" | "E=>" => Ok(CmpOp::Ge),
        "E<=" | "E=<" => Ok(CmpOp::Le),
        _ => Err(format!("Unknown comparison operator: {}", op)),
    }
}

fn parse_condition(cond_str: &str) -> Result<(Expr, CmpOp, Expr), String> {
    let cond_str = cond_str.trim();
    let bytes = cond_str.as_bytes();
    let mut in_str = false;
    let mut depth = 0usize;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            depth = depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if depth == 0 && b == b'E' {
            let rest = &cond_str[i..];
            for op_str in ["E!=", "E>=", "E<=", "E=>", "E=<", "E>", "E<", "E"] {
                if rest.starts_with(op_str) {
                    let left_str = &cond_str[..i];
                    let right_str = &cond_str[i + op_str.len()..];
                    if !left_str.trim().is_empty() && !right_str.trim().is_empty() {
                        let left = parse_expr(left_str)?;
                        let op = parse_cmp_op(op_str)?;
                        let right = parse_expr(right_str)?;
                        return Ok((left, op, right));
                    }
                }
            }
        }
        i += 1;
    }

    if let Some(caps) = cond_regex().captures(cond_str) {
        let left = parse_expr(caps.get(1).unwrap().as_str())?;
        let op = parse_cmp_op(caps.get(2).unwrap().as_str())?;
        let right = parse_expr(caps.get(3).unwrap().as_str())?;
        return Ok((left, op, right));
    }

    Err(format!("Invalid condition: {}", cond_str))
}

fn stmt_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pat = r#"^(?:Tht\s*\(|Th\s*\(|The\s*\(|T\s*\{|Im\s*\(|Sy\s*\(|Cw(?:\s|V|F|G|W|M|T|I|\(|"|\d)|Cf\s*\(|I(?:\s|V|F|G|W|M|T|S|\(|"|\d)|Fl\s*"(?:[^"\\]|\\.)*"\s*(?:\{|\=|\()|F\s*"(?:[^"\\]|\\.)*"\s*(?:\{|\=|\()|Fl\s*\d+\s*(?:\{|\=|\()|F\s*\d+\s*(?:\{|\=|\()|Vl\s*"(?:[^"\\]|\\.)*"\s*=|V\s*"(?:[^"\\]|\\.)*"\s*=|Vl\s*\d+\s*=|V\s*\d+\s*=|Rs(?:\s|V)|R(?:\s|V)|Ls\s*D\s*\d+|Lp\s*D\s*\d+|Z\s*D\s*\d+|A(?:\s|V|F|G|W|M|T|S|I|\(|"|\d)|Mh\s*=|P(?:\s|V|F|G|W|M|T|S|I|\(|"|\d)|S(?:\s|V|F|G|W|M|T|I|\(|"|\d))"#;
        Regex::new(pat).unwrap()
    })
}

fn parse_block(src: &str) -> Result<Vec<Stmt>, String> {
    let cleaned = strip_comments_and_whitespace(src, true);
    let mut stmts = Vec::new();
    let mut pos = 0;
    let len = cleaned.len();

    while pos < len {
        while pos < len {
            let ch = cleaned[pos..].chars().next().unwrap();
            if ch.is_whitespace() || ch == ';' {
                pos += ch.len_utf8();
            } else {
                break;
            }
        }
        if pos >= len {
            break;
        }

        let rem = &cleaned[pos..];

        // 1. TailThread: Tht(...)
        if let Some(after) = rem.strip_prefix("Tht") {
            let trimmed = after.trim_start();
            if trimmed.starts_with('(') {
                let open = rem.len() - trimmed.len();
                let close = find_matching_delim(rem, open, '(', ')')?;
                let id_expr = parse_expr(&rem[open + 1..close])?;
                stmts.push(Stmt::TailThread(id_expr));
                pos += close + 1;
                continue;
            }
        }

        // 2. EndThread: The(...)
        if let Some(after) = rem.strip_prefix("The") {
            let trimmed = after.trim_start();
            if trimmed.starts_with('(') {
                let open = rem.len() - trimmed.len();
                let close = find_matching_delim(rem, open, '(', ')')?;
                let id_expr = parse_expr(&rem[open + 1..close])?;
                stmts.push(Stmt::EndThread(id_expr));
                pos += close + 1;
                continue;
            }
        }

        // 3. SpawnThread: Th(...){...}
        if let Some(after) = rem.strip_prefix("Th") {
            let trimmed = after.trim_start();
            if trimmed.starts_with('(') {
                let open_paren = rem.len() - trimmed.len();
                let close_paren = find_matching_delim(rem, open_paren, '(', ')')?;
                let id_expr = parse_expr(&rem[open_paren + 1..close_paren])?;
                let after_paren = rem[close_paren + 1..].trim_start();
                let offset = rem.len() - after_paren.len();
                if !after_paren.starts_with('{') {
                    return Err(format!("Expected '{{' after Th(...) near: {}", &rem[..rem.len().min(40)]));
                }
                let close_brace = find_matching_delim(rem, offset, '{', '}')?;
                let body = parse_block(&rem[offset + 1..close_brace])?;
                stmts.push(Stmt::SpawnThread { id: id_expr, body });
                pos += close_brace + 1;
                continue;
            }
        }

        // 4. TryCatch: T{...}K{...}
        if rem.starts_with('T') && rem[1..].trim_start().starts_with('{') {
            let t_open = rem.find('{').unwrap();
            let t_close = find_matching_delim(rem, t_open, '{', '}')?;
            let try_body = parse_block(&rem[t_open + 1..t_close])?;
            let after_t = rem[t_close + 1..].trim_start();
            if !after_t.starts_with('K') {
                return Err(format!("Expected 'K{{...}}' after T{{...}} near: {}", &rem[..rem.len().min(40)]));
            }
            let k_offset = rem.len() - after_t.len();
            let k_open_rel = rem[k_offset + 1..].find('{').ok_or("Expected '{' after K")?;
            let k_open = k_offset + 1 + k_open_rel;
            let k_close = find_matching_delim(rem, k_open, '{', '}')?;
            let catch_body = parse_block(&rem[k_open + 1..k_close])?;
            stmts.push(Stmt::TryCatch { try_body, catch_body });
            pos += k_close + 1;
            continue;
        }

        // 5. WhileLoop: Cw <cond> { ... }
        if rem.starts_with("Cw") {
            let open_brace = find_char_outside_quotes(rem, '{').ok_or("Expected '{' in Cw loop")?;
            let close_brace = find_matching_delim(rem, open_brace, '{', '}')?;
            let cond_str = &rem[2..open_brace];
            let (left, op, right) = parse_condition(cond_str)?;
            let body = parse_block(&rem[open_brace + 1..close_brace])?;

            if let (Expr::Var(VarTarget::GlobalNum(idx)), Expr::Lit(Value::Int(limit))) = (&left, &right) {
                if body.len() == 1 {
                    if let Stmt::AddConstInt { idx: b_idx, delta } = body[0] {
                        if *idx == b_idx {
                            stmts.push(Stmt::IntLoop {
                                idx: *idx,
                                op,
                                limit: *limit,
                                body: vec![Stmt::AddConstInt { idx: b_idx, delta }],
                            });
                            pos += close_brace + 1;
                            continue;
                        }
                    }
                }
            }

            stmts.push(Stmt::WhileLoop { left, op, right, body });
            pos += close_brace + 1;
            continue;
        }

        // 6. CountLoop: Cf(count){...}
        if let Some(after) = rem.strip_prefix("Cf") {
            let trimmed = after.trim_start();
            if trimmed.starts_with('(') {
                let open_paren = rem.len() - trimmed.len();
                let close_paren = find_matching_delim(rem, open_paren, '(', ')')?;
                let count_expr = parse_expr(&rem[open_paren + 1..close_paren])?;
                let after_paren = rem[close_paren + 1..].trim_start();
                let offset = rem.len() - after_paren.len();
                if !after_paren.starts_with('{') {
                    return Err("Expected '{' after Cf(...)".to_string());
                }
                let close_brace = find_matching_delim(rem, offset, '{', '}')?;
                let body = parse_block(&rem[offset + 1..close_brace])?;
                stmts.push(Stmt::CountLoop { count: count_expr, body });
                pos += close_brace + 1;
                continue;
            }
        }

        // 7. IfChain: I <cond> { ... } [J <cond> { ... }]* [H { ... }]
        if rem.starts_with('I') && !rem.starts_with("Im") {
            if let Some(open_brace) = find_char_outside_quotes(rem, '{') {
                let cond_candidate = rem[1..open_brace].trim();
                if !cond_candidate.is_empty() && !cond_candidate.starts_with('=') {
                    let mut branches = Vec::new();
                    let mut else_body = None;

                    let close_brace = find_matching_delim(rem, open_brace, '{', '}')?;
                    let (left, op, right) = parse_condition(cond_candidate)?;
                    let body = parse_block(&rem[open_brace + 1..close_brace])?;
                    branches.push((left, op, right, body));
                    let mut cur = close_brace + 1;

                    loop {
                        let tail = rem[cur..].trim_start();
                        let skipped = rem[cur..].len() - tail.len();
                        if tail.starts_with('J') {
                            let j_start = cur + skipped;
                            let j_open = find_char_outside_quotes(&rem[j_start..], '{').ok_or("Expected '{' after J")? + j_start;
                            let j_close = find_matching_delim(rem, j_open, '{', '}')?;
                            let (jl, jop, jr) = parse_condition(&rem[j_start + 1..j_open])?;
                            let jbody = parse_block(&rem[j_open + 1..j_close])?;
                            branches.push((jl, jop, jr, jbody));
                            cur = j_close + 1;
                        } else if tail.starts_with('H') && tail[1..].trim_start().starts_with('{') {
                            let h_start = cur + skipped;
                            let h_open = find_char_outside_quotes(&rem[h_start..], '{').ok_or("Expected '{' after H")? + h_start;
                            let h_close = find_matching_delim(rem, h_open, '{', '}')?;
                            else_body = Some(parse_block(&rem[h_open + 1..h_close])?);
                            cur = h_close + 1;
                            break;
                        } else {
                            break;
                        }
                    }

                    stmts.push(Stmt::IfChain { branches, else_body });
                    pos += cur;
                    continue;
                }
            }
        }

        // 8. Import: Im(...)
        if let Some(after) = rem.strip_prefix("Im") {
            let trimmed = after.trim_start();
            if trimmed.starts_with('(') {
                let open = rem.len() - trimmed.len();
                let close = find_matching_delim(rem, open, '(', ')')?;
                let path_expr = parse_expr(&rem[open + 1..close])?;
                stmts.push(Stmt::Import(path_expr));
                pos += close + 1;
                continue;
            }
        }

        // 9. SysExec: Sy(...)
        if let Some(after) = rem.strip_prefix("Sy") {
            let trimmed = after.trim_start();
            if trimmed.starts_with('(') {
                let open = rem.len() - trimmed.len();
                let close = find_matching_delim(rem, open, '(', ')')?;
                let cmd_expr = parse_expr(&rem[open + 1..close])?;
                stmts.push(Stmt::SysExec(cmd_expr));
                pos += close + 1;
                continue;
            }
        }

        // 10. Function definition or call: F0{...}, F"name"{...}, F0=+1/2, F"name"()
        if rem.starts_with("Fl") || rem.starts_with('F') {
            let mut in_str = false;
            let mut idx = 0;
            let bytes = rem.as_bytes();
            while idx < bytes.len() {
                if in_str {
                    if bytes[idx] == b'\\' && idx + 1 < bytes.len() {
                        idx += 2;
                        continue;
                    }
                    if bytes[idx] == b'"' {
                        in_str = false;
                    }
                    idx += 1;
                    continue;
                }
                if bytes[idx] == b'"' {
                    in_str = true;
                    idx += 1;
                    continue;
                }
                if matches!(bytes[idx], b'{' | b'=' | b'(') {
                    break;
                }
                idx += 1;
            }
            if idx < bytes.len() {
                let target = parse_func_target(&rem[..idx])?;
                let kind_ch = bytes[idx] as char;
                if kind_ch == '{' {
                    let close_brace = find_matching_delim(rem, idx, '{', '}')?;
                    let body = parse_block(&rem[idx + 1..close_brace])?;
                    stmts.push(Stmt::FuncDef {
                        target,
                        def: FuncDef::Block(body),
                    });
                    pos += close_brace + 1;
                    continue;
                } else if kind_ch == '(' {
                    let close_paren = find_matching_delim(rem, idx, '(', ')')?;
                    let arg_str = rem[idx + 1..close_paren].trim();
                    let arg = if arg_str.is_empty() { None } else { Some(parse_expr(arg_str)?) };
                    stmts.push(Stmt::FuncExec { target, arg });
                    pos += close_paren + 1;
                    continue;
                } else if kind_ch == '=' {
                    let end = find_stmt_end(rem, idx + 1);
                    let pipe_str = rem[idx + 1..end].trim();
                    let mut steps = Vec::new();
                    let mut pi = 0;
                    let pbytes = pipe_str.as_bytes();
                    while pi < pbytes.len() {
                        while pi < pbytes.len() && pbytes[pi].is_ascii_whitespace() {
                            pi += 1;
                        }
                        if pi >= pbytes.len() {
                            break;
                        }
                        let op = (pbytes[pi] as char).to_string();
                        pi += 1;
                        let start_val = pi;
                        while pi < pbytes.len() && !matches!(pbytes[pi], b'+' | b'-' | b'*' | b'/' | b'%' | b'^') {
                            pi += 1;
                        }
                        let val_expr = parse_expr(&pipe_str[start_val..pi])?;
                        steps.push((op, val_expr));
                    }
                    stmts.push(Stmt::FuncDef {
                        target,
                        def: FuncDef::LegacyPipeline(steps),
                    });
                    pos += end;
                    continue;
                }
            }
        }

        // 11. Angle Mode: Mh=radians / Mh=degrees
        if let Some(rest) = rem.strip_prefix("Mh") {
            let trimmed = rest.trim_start();
            if let Some(after_eq) = trimmed.strip_prefix('=') {
                let val_part = after_eq.trim_start();
                if val_part.starts_with("radians") {
                    let consumed = rem.len() - val_part.len() + "radians".len();
                    stmts.push(Stmt::AngleMode("radians".to_string()));
                    pos += consumed;
                    continue;
                } else if val_part.starts_with("degrees") {
                    let consumed = rem.len() - val_part.len() + "degrees".len();
                    stmts.push(Stmt::AngleMode("degrees".to_string()));
                    pos += consumed;
                    continue;
                }
            }
        }

        // 12. Random: Rs V0,1,10 or R V0,1,10
        if rem.starts_with("Rs") || (rem.starts_with('R') && !rem.starts_with("R\"")) {
            let secure = rem.starts_with("Rs");
            let prefix_len = if secure { 2 } else { 1 };
            let after = rem[prefix_len..].trim_start();
            let offset = rem.len() - after.len();
            let end = find_stmt_end(rem, offset);
            let spec = rem[offset..end].trim();
            let parts: Vec<&str> = spec.splitn(3, ',').collect();
            if parts.len() != 3 {
                return Err(format!("Invalid random syntax (expected R target,min,max): {}", spec));
            }
            let target = parse_var_target(parts[0])?;
            let min = parse_expr(parts[1])?;
            let max = parse_expr(parts[2])?;
            stmts.push(Stmt::Rand { target, min, max, secure });
            pos += end;
            continue;
        }

        // 13. Binary Save / Load: Ls D1 "name" or LsD1"name"
        if rem.starts_with("Ls") || rem.starts_with("Lp") {
            let is_save = rem.starts_with("Ls");
            let after = rem[2..].trim_start();
            if let Some(after_d) = after.strip_prefix('D') {
                let after_d_trim = after_d.trim_start();
                let digits_end = after_d_trim.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_d_trim.len());
                let dict_idx = after_d_trim[..digits_end].parse::<usize>().map_err(|_| "Invalid dict in Ls/Lp")?;
                let name_part = after_d_trim[digits_end..].trim_start();
                let name_offset = rem.len() - name_part.len();
                let end = find_stmt_end(rem, name_offset);
                let name_expr = parse_expr(&rem[name_offset..end])?;
                if is_save {
                    stmts.push(Stmt::SaveBin { dict: dict_idx, name: name_expr });
                } else {
                    stmts.push(Stmt::LoadBin { dict: dict_idx, name: name_expr });
                }
                pos += end;
                continue;
            }
        }

        // 14. Clear Dictionary: Z D1 or ZD1
        if rem.starts_with('Z') {
            let after = rem[1..].trim_start();
            if let Some(after_d) = after.strip_prefix('D') {
                let after_d_trim = after_d.trim_start();
                let digits_end = after_d_trim.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_d_trim.len());
                if digits_end > 0 {
                    let dict_idx = after_d_trim[..digits_end].parse::<usize>().map_err(|_| "Invalid dict in Z")?;
                    let consumed = rem.len() - after_d_trim.len() + digits_end;
                    stmts.push(Stmt::DictClear(dict_idx));
                    pos += consumed;
                    continue;
                }
            }
        }

        // 15. Append to Dictionary: A <expr> D1
        if rem.starts_with('A') {
            let after = rem[1..].trim_start();
            let offset = rem.len() - after.len();
            let end = find_stmt_end(rem, offset);
            let slice = rem[offset..end].trim();
            // Find trailing D<num> outside quotes
            let mut in_str = false;
            let mut last_d = None;
            for (idx, b) in slice.as_bytes().iter().enumerate() {
                if in_str {
                    if *b == b'"' {
                        in_str = false;
                    }
                    continue;
                }
                if *b == b'"' {
                    in_str = true;
                } else if *b == b'D' {
                    last_d = Some(idx);
                }
            }
            if let Some(d_pos) = last_d {
                let dict_digits = slice[d_pos + 1..].trim();
                if !dict_digits.is_empty() && dict_digits.chars().all(|c| c.is_ascii_digit()) {
                    let dict_idx = dict_digits.parse::<usize>().unwrap();
                    let val_expr = parse_expr(&slice[..d_pos])?;
                    stmts.push(Stmt::DictAppend { val: val_expr, dict: dict_idx });
                    pos += end;
                    continue;
                }
            }
        }

        // 16. Print: P <expr> or P<expr>
        if rem.starts_with('P') {
            let after = rem[1..].trim_start();
            let offset = rem.len() - after.len();
            let end = find_stmt_end(rem, offset);
            let expr = parse_expr(&rem[offset..end])?;
            stmts.push(Stmt::Print(expr));
            pos += end;
            continue;
        }

        // 17. Sleep: S <expr> or S<expr>
        if rem.starts_with('S') && !rem.starts_with("Sy") {
            let after = rem[1..].trim_start();
            let offset = rem.len() - after.len();
            let end = find_stmt_end(rem, offset);
            let expr = parse_expr(&rem[offset..end])?;
            stmts.push(Stmt::Sleep(expr));
            pos += end;
            continue;
        }

        // 18. Variable Assignment: V0=..., V"name"=..., Vl0=..., Vl"name"=...
        if rem.starts_with("Vl") || rem.starts_with('V') {
            if let Some(eq_pos) = find_char_outside_quotes(rem, '=') {
                let target = parse_var_target(&rem[..eq_pos])?;
                let val_start = eq_pos + 1;
                let end = find_stmt_end(rem, val_start);
                let expr = parse_expr(&rem[val_start..end])?;

                if let (VarTarget::GlobalNum(idx), Expr::BinOp { left, op, right }) = (&target, &expr) {
                    if let (Expr::Var(VarTarget::GlobalNum(l_idx)), Expr::Lit(Value::Int(k))) = (&**left, &**right) {
                        if *idx == *l_idx && (op == "+" || op == "-") {
                            let delta = if op == "+" { *k } else { -*k };
                            stmts.push(Stmt::AddConstInt { idx: *idx, delta });
                            pos += end;
                            continue;
                        }
                    }
                }

                stmts.push(Stmt::Assign { target, expr });
                pos += end;
                continue;
            }
        }

        let preview: String = rem.chars().take(40).collect();
        return Err(format!("Cannot parse near: {:?}", preview));
    }

    Ok(stmts)
}

fn find_stmt_end(rem: &str, start: usize) -> usize {
    let bytes = rem.as_bytes();
    let mut in_str = false;
    let mut depth = 0usize;
    let mut i = start;

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }

    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            if depth == 0 && i > start {
                let prev = rem[start..i].trim_end();
                let is_named_ident = prev.ends_with("Vl")
                    || prev.ends_with('V')
                    || prev.ends_with("Fl")
                    || prev.ends_with('F');
                if !is_named_ident {
                    if let Some(last_ch) = prev.chars().last() {
                        if !matches!(last_ch, '+' | '-' | '*' | '/' | '%' | '^' | 'M' | ',' | '(' | '=') {
                            return i;
                        }
                    }
                }
            }
            in_str = true;
            i += 1;
            continue;
        }
        if b == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            if depth > 0 {
                depth -= 1;
                i += 1;
                continue;
            } else {
                return i;
            }
        }
        if depth == 0 {
            if b == b';' || b == b'}' {
                return i;
            }
            if i > start {
                let prev = rem[start..i].trim_end();
                if let Some(last_ch) = prev.chars().last() {
                    if !matches!(last_ch, '+' | '-' | '*' | '/' | '%' | '^' | 'M' | ',' | '(' | '=') {
                        let sub = &rem[i..];
                        if stmt_regex().is_match(sub) {
                            return i;
                        }
                    }
                }
            }
        }
        i += 1;
    }
    bytes.len()
}

// ============================================================================
// PEMDAS EVALUATOR (Mf)
// ============================================================================

fn eval_pemdas(expr_str: &str, gs: &Arc<Mutex<GlobalState>>) -> Result<Value, String> {
    let resolved = resolve_pemdas_atoms(expr_str, gs)?;
    let val = parse_pemdas_add_sub(&resolved, &mut 0)?;
    if val.fract() == 0.0 && val.abs() < (i64::MAX as f64) {
        Ok(Value::Int(val as i64))
    } else {
        Ok(Value::Float(val))
    }
}

fn resolve_pemdas_atoms(s: &str, gs: &Arc<Mutex<GlobalState>>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if matches!(c, '+' | '-' | '*' | '/' | '%' | '^' | '(' | ')') {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0usize;
        let mut in_str = false;
        while i < bytes.len() {
            let b = bytes[i];
            if in_str {
                if b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            if b == b'"' {
                in_str = true;
                i += 1;
                continue;
            }
            if b == b'(' {
                depth += 1;
                i += 1;
                continue;
            }
            if b == b')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                i += 1;
                continue;
            }
            if depth == 0 && matches!(b, b'+' | b'-' | b'*' | b'/' | b'%' | b'^') {
                break;
            }
            i += 1;
        }
        let atom_tok = &s[start..i];
        let atom_expr = parse_atom(atom_tok)?;
        let num = eval_expr(&atom_expr, gs)?.as_f64()?;
        out.push_str(&format!("({})", num));
    }
    Ok(out)
}

fn parse_pemdas_add_sub(s: &str, pos: &mut usize) -> Result<f64, String> {
    let mut left = parse_pemdas_mul_div(s, pos)?;
    let bytes = s.as_bytes();
    while *pos < bytes.len() {
        let op = bytes[*pos] as char;
        if op == '+' || op == '-' {
            *pos += 1;
            let right = parse_pemdas_mul_div(s, pos)?;
            if op == '+' {
                left += right;
            } else {
                left -= right;
            }
        } else {
            break;
        }
    }
    Ok(left)
}

fn parse_pemdas_mul_div(s: &str, pos: &mut usize) -> Result<f64, String> {
    let mut left = parse_pemdas_pow(s, pos)?;
    let bytes = s.as_bytes();
    while *pos < bytes.len() {
        let op = bytes[*pos] as char;
        if op == '*' || op == '/' || op == '%' {
            *pos += 1;
            let right = parse_pemdas_pow(s, pos)?;
            if op == '*' {
                left *= right;
            } else if op == '/' {
                if right == 0.0 {
                    return Err("Division by zero".to_string());
                }
                left /= right;
            } else {
                if right == 0.0 {
                    return Err("Modulo by zero".to_string());
                }
                left %= right;
            }
        } else {
            break;
        }
    }
    Ok(left)
}

fn parse_pemdas_pow(s: &str, pos: &mut usize) -> Result<f64, String> {
    let base = parse_pemdas_primary(s, pos)?;
    let bytes = s.as_bytes();
    if *pos < bytes.len() && bytes[*pos] == b'^' {
        *pos += 1;
        let exp = parse_pemdas_pow(s, pos)?;
        return Ok(base.powf(exp));
    }
    Ok(base)
}

fn parse_pemdas_primary(s: &str, pos: &mut usize) -> Result<f64, String> {
    let bytes = s.as_bytes();
    if *pos >= bytes.len() {
        return Err("Unexpected end of PEMDAS expression".to_string());
    }
    if bytes[*pos] == b'-' {
        *pos += 1;
        return Ok(-parse_pemdas_primary(s, pos)?);
    }
    if bytes[*pos] == b'+' {
        *pos += 1;
        return parse_pemdas_primary(s, pos);
    }
    if bytes[*pos] == b'(' {
        *pos += 1;
        let val = parse_pemdas_add_sub(s, pos)?;
        if *pos >= bytes.len() || bytes[*pos] != b')' {
            return Err("Missing closing ')' in PEMDAS expression".to_string());
        }
        *pos += 1;
        return Ok(val);
    }
    let start = *pos;
    while *pos < bytes.len()
        && (bytes[*pos].is_ascii_digit()
            || bytes[*pos] == b'.'
            || bytes[*pos] == b'e'
            || bytes[*pos] == b'E'
            || ((*pos > start)
                && (bytes[*pos - 1] == b'e' || bytes[*pos - 1] == b'E')
                && (bytes[*pos] == b'+' || bytes[*pos] == b'-')))
    {
        *pos += 1;
    }
    s[start..*pos]
        .parse::<f64>()
        .map_err(|_| format!("Invalid number in PEMDAS: {}", &s[start..*pos]))
}

// ============================================================================
// EVALUATOR & RUNTIME EXECUTION
// ============================================================================

fn read_var(target: &VarTarget, gs: &Arc<Mutex<GlobalState>>) -> Value {
    match target {
        VarTarget::GlobalNum(idx) => {
            let st = gs.lock().unwrap();
            st.v.get(*idx).cloned().unwrap_or(Value::Null)
        }
        VarTarget::GlobalNamed(name) => {
            let st = gs.lock().unwrap();
            st.v_named.get(name).cloned().unwrap_or(Value::Null)
        }
        VarTarget::LocalNum(idx) => LOCAL_ENV.with(|env| env.borrow().vl.get(*idx).cloned().unwrap_or(Value::Null)),
        VarTarget::LocalNamed(name) => {
            LOCAL_ENV.with(|env| env.borrow().vl_named.get(name).cloned().unwrap_or(Value::Null))
        }
    }
}

fn write_var(target: &VarTarget, val: Value, gs: &Arc<Mutex<GlobalState>>) {
    match target {
        VarTarget::GlobalNum(idx) => {
            let mut st = gs.lock().unwrap();
            if *idx >= st.v.len() {
                st.v.resize(*idx + 1, Value::Null);
            }
            st.v[*idx] = val;
        }
        VarTarget::GlobalNamed(name) => {
            let mut st = gs.lock().unwrap();
            st.v_named.insert(name.clone(), val);
        }
        VarTarget::LocalNum(idx) => LOCAL_ENV.with(|env| {
            let mut e = env.borrow_mut();
            if *idx >= e.vl.len() {
                e.vl.resize(*idx + 1, Value::Null);
            }
            e.vl[*idx] = val;
        }),
        VarTarget::LocalNamed(name) => LOCAL_ENV.with(|env| {
            env.borrow_mut().vl_named.insert(name.clone(), val);
        }),
    }
}

fn eval_expr(expr: &Expr, gs: &Arc<Mutex<GlobalState>>) -> Result<Value, String> {
    match expr {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Var(t) => Ok(read_var(t, gs)),
        Expr::Input => {
            io::stdout().flush().ok();
            let mut line = String::new();
            io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
            let trimmed = line.trim();
            if let Ok(i) = trimmed.parse::<i64>() {
                Ok(Value::Int(i))
            } else if let Ok(f) = trimmed.parse::<f64>() {
                Ok(Value::Float(f))
            } else {
                Ok(Value::Str(trimmed.to_string()))
            }
        }
        Expr::SysCall(cmd_expr) => {
            let cmd_str = eval_expr(cmd_expr, gs)?.display_str();
            let output = if cfg!(target_os = "windows") {
                Command::new("cmd").args(["/C", &cmd_str]).output()
            } else {
                Command::new("sh").args(["-c", &cmd_str]).output()
            }
            .map_err(|e| format!("System command failed: {}", e))?;
            Ok(Value::Str(String::from_utf8_lossy(&output.stdout).trim_end().to_string()))
        }
        Expr::TimeElapsed => {
            let st = gs.lock().unwrap();
            Ok(Value::Float(st.start_time.elapsed().as_secs_f64()))
        }
        Expr::TimeUnix => {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            Ok(Value::Float(secs))
        }
        Expr::DictGet { idx, dict } => {
            let i = eval_expr(idx, gs)?.as_i64()?;
            if i <= 0 {
                return Err(format!("Dictionary indices are 1-based (got {})", i));
            }
            let st = gs.lock().unwrap();
            let vec = st.d.get(*dict).ok_or_else(|| format!("Invalid dictionary D{}", dict))?;
            vec.get((i - 1) as usize)
                .cloned()
                .ok_or_else(|| format!("Index {} out of bounds for D{}", i, dict))
        }
        Expr::DictFind { val, dict } => {
            let target_val = eval_expr(val, gs)?;
            let st = gs.lock().unwrap();
            let vec = st.d.get(*dict).ok_or_else(|| format!("Invalid dictionary D{}", dict))?;
            for (pos, item) in vec.iter().enumerate() {
                if item == &target_val || item.display_str() == target_val.display_str() {
                    return Ok(Value::Int((pos + 1) as i64));
                }
            }
            Ok(Value::Int(0))
        }
        Expr::Pemdas(raw) => eval_pemdas(raw, gs),
        Expr::Trig { kind, arg } => {
            let raw_val = eval_expr(arg, gs)?.as_f64()?;
            let is_rad = gs.lock().unwrap().angle_radians;
            let rad = if is_rad { raw_val } else { raw_val.to_radians() };
            let res = match kind {
                's' => rad.sin(),
                'c' => rad.cos(),
                't' => rad.tan(),
                'a' => {
                    let a = raw_val.asin();
                    if is_rad { a } else { a.to_degrees() }
                }
                _ => return Err(format!("Unknown trig function M{}", kind)),
            };
            Ok(Value::Float(res))
        }
        Expr::Round(arg) => {
            let v = eval_expr(arg, gs)?.as_f64()?;
            Ok(Value::Float((v * 10000.0).round() / 10000.0))
        }
        Expr::FuncCall { target, arg } => {
            let arg_val = match arg {
                Some(a) => Some(eval_expr(a, gs)?),
                None => None,
            };
            invoke_func(target, arg_val, gs)
        }
        Expr::BinOp { left, op, right } => {
            let lval = eval_expr(left, gs)?;
            let rval = eval_expr(right, gs)?;

            if op == "+" {
                if let (Value::Str(a), b) = (&lval, &rval) {
                    return Ok(Value::Str(format!("{}{}", a, b.display_str())));
                }
                if let (a, Value::Str(b)) = (&lval, &rval) {
                    return Ok(Value::Str(format!("{}{}", a.display_str(), b)));
                }
            }

            if let (Value::Int(a), Value::Int(b)) = (&lval, &rval) {
                return match op.as_str() {
                    "+" => Ok(Value::Int(a.wrapping_add(*b))),
                    "-" => Ok(Value::Int(a.wrapping_sub(*b))),
                    "*" => Ok(Value::Int(a.wrapping_mul(*b))),
                    "/" => {
                        if *b == 0 {
                            Err("Division by zero".to_string())
                        } else if a % b == 0 {
                            Ok(Value::Int(a / b))
                        } else {
                            Ok(Value::Float(*a as f64 / *b as f64))
                        }
                    }
                    "%" | "M" => {
                        if *b == 0 {
                            Err("Modulo by zero".to_string())
                        } else {
                            Ok(Value::Int(a % b))
                        }
                    }
                    "^" => Ok(Value::Float((*a as f64).powf(*b as f64))),
                    _ => Err(format!("Unknown operator: {}", op)),
                };
            }

            let a = lval.as_f64()?;
            let b = rval.as_f64()?;
            match op.as_str() {
                "+" => Ok(Value::Float(a + b)),
                "-" => Ok(Value::Float(a - b)),
                "*" => Ok(Value::Float(a * b)),
                "/" => {
                    if b == 0.0 {
                        Err("Division by zero".to_string())
                    } else {
                        Ok(Value::Float(a / b))
                    }
                }
                "%" | "M" => {
                    if b == 0.0 {
                        Err("Modulo by zero".to_string())
                    } else {
                        Ok(Value::Float(a % b))
                    }
                }
                "^" => Ok(Value::Float(a.powf(b))),
                _ => Err(format!("Unknown operator: {}", op)),
            }
        }
    }
}

fn eval_cmp(left: &Expr, op: &CmpOp, right: &Expr, gs: &Arc<Mutex<GlobalState>>) -> Result<bool, String> {
    let lval = eval_expr(left, gs)?;
    let rval = eval_expr(right, gs)?;

    if let (Value::Str(a), Value::Str(b)) = (&lval, &rval) {
        return Ok(match op {
            CmpOp::Eq => a == b,
            CmpOp::NotEq => a != b,
            CmpOp::Gt => a > b,
            CmpOp::Lt => a < b,
            CmpOp::Ge => a >= b,
            CmpOp::Le => a <= b,
        });
    }

    if let (Value::Bool(a), Value::Bool(b)) = (&lval, &rval) {
        return Ok(match op {
            CmpOp::Eq => a == b,
            CmpOp::NotEq => a != b,
            _ => false,
        });
    }

    let a = lval.as_f64()?;
    let b = rval.as_f64()?;
    Ok(match op {
        CmpOp::Eq => (a - b).abs() < 1e-12,
        CmpOp::NotEq => (a - b).abs() >= 1e-12,
        CmpOp::Gt => a > b,
        CmpOp::Lt => a < b,
        CmpOp::Ge => a >= b,
        CmpOp::Le => a <= b,
    })
}

fn invoke_func(target: &FuncTarget, arg: Option<Value>, gs: &Arc<Mutex<GlobalState>>) -> Result<Value, String> {
    let def = match target {
        FuncTarget::GlobalNum(idx) => gs.lock().unwrap().f.get(idx).cloned(),
        FuncTarget::GlobalNamed(name) => gs.lock().unwrap().f_named.get(name).cloned(),
        FuncTarget::LocalNum(idx) => LOCAL_ENV.with(|env| env.borrow().fl.get(idx).cloned()),
        FuncTarget::LocalNamed(name) => LOCAL_ENV.with(|env| env.borrow().fl_named.get(name).cloned()),
    }
    .ok_or_else(|| format!("Undefined function: {:?}", target))?;

    if let Some(val) = arg {
        LOCAL_ENV.with(|env| {
            env.borrow_mut().vl[0] = val;
        });
    }

    match def {
        FuncDef::LegacyPipeline(steps) => {
            let mut acc = LOCAL_ENV.with(|env| env.borrow().vl[0].as_f64()).unwrap_or(0.0);
            for (op, expr) in steps {
                let rhs = eval_expr(&expr, gs)?.as_f64()?;
                match op.as_str() {
                    "+" => acc += rhs,
                    "-" => acc -= rhs,
                    "*" => acc *= rhs,
                    "/" => {
                        if rhs == 0.0 {
                            return Err("Division by zero".to_string());
                        }
                        acc /= rhs;
                    }
                    "%" => acc %= rhs,
                    "^" => acc = acc.powf(rhs),
                    _ => {}
                }
            }
            let res = if acc.fract() == 0.0 { Value::Int(acc as i64) } else { Value::Float(acc) };
            LOCAL_ENV.with(|env| env.borrow_mut().vl[0] = res.clone());
            Ok(res)
        }
        FuncDef::Block(stmts) => {
            let mut out = BufWriter::with_capacity(65536, io::stdout());
            exec_stmts(&stmts, gs, &mut out)?;
            out.flush().ok();
            Ok(LOCAL_ENV.with(|env| env.borrow().vl[0].clone()))
        }
    }
}

fn exec_stmts<W: Write>(stmts: &[Stmt], gs: &Arc<Mutex<GlobalState>>, out: &mut W) -> Result<(), String> {
    let mut i = 0;
    while i < stmts.len() {
        let cancelled = LOCAL_ENV.with(|env| {
            env.borrow()
                .cancel_flag
                .as_ref()
                .map_or(false, |f| f.load(Ordering::Relaxed))
        });
        if cancelled {
            return Ok(());
        }

        match &stmts[i] {
            Stmt::Assign { target, expr } => {
                let val = eval_expr(expr, gs)?;
                write_var(target, val, gs);
            }
            Stmt::AddConstInt { idx, delta } => {
                let mut st = gs.lock().unwrap();
                if let Some(Value::Int(cur)) = st.v.get_mut(*idx) {
                    *cur = cur.wrapping_add(*delta);
                } else {
                    let cur_num = st.v.get(*idx).and_then(|v| v.as_i64().ok()).unwrap_or(0);
                    if *idx >= st.v.len() {
                        st.v.resize(*idx + 1, Value::Null);
                    }
                    st.v[*idx] = Value::Int(cur_num.wrapping_add(*delta));
                }
            }
            Stmt::Print(expr) => {
                let val = eval_expr(expr, gs)?;
                writeln!(out, "{}", val.display_str()).map_err(|e| e.to_string())?;
                out.flush().ok();
            }
            Stmt::Sleep(expr) => {
                out.flush().ok();
                let secs = eval_expr(expr, gs)?.as_f64()?;
                if secs > 0.0 {
                    thread::sleep(Duration::from_secs_f64(secs));
                }
            }
            Stmt::Import(expr) => {
                let rel_str = eval_expr(expr, gs)?.display_str();
                let full_path = {
                    let st = gs.lock().unwrap();
                    let p = Path::new(&rel_str);
                    if p.is_absolute() { p.to_path_buf() } else { st.base_dir.join(p) }
                };
                let canonical = fs::canonicalize(&full_path).unwrap_or_else(|_| full_path.clone());
                let already = {
                    let mut st = gs.lock().unwrap();
                    if st.imported_files.contains(&canonical) {
                        true
                    } else {
                        st.imported_files.insert(canonical.clone());
                        false
                    }
                };
                if !already {
                    let code = fs::read_to_string(&full_path)
                        .map_err(|e| format!("Failed to import '{}': {}", full_path.display(), e))?;
                    let imported_stmts = parse_block(&code)?;
                    exec_stmts(&imported_stmts, gs, out)?;
                }
            }
            Stmt::SysExec(expr) => {
                let cmd_str = eval_expr(expr, gs)?.display_str();
                if cfg!(target_os = "windows") {
                    Command::new("cmd")
                        .args(["/C", &cmd_str])
                        .status()
                        .map_err(|e| format!("System command failed: {}", e))?;
                } else {
                    Command::new("sh")
                        .args(["-c", &cmd_str])
                        .status()
                        .map_err(|e| format!("System command failed: {}", e))?;
                }
            }
            Stmt::Rand { target, min, max, secure } => {
                let lo = eval_expr(min, gs)?.as_i64()?;
                let hi = eval_expr(max, gs)?.as_i64()?;
                let (low, high) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                let r = if *secure {
                    OsRng.gen_range(low..=high)
                } else {
                    rand::thread_rng().gen_range(low..=high)
                };
                write_var(target, Value::Int(r), gs);
            }
            Stmt::DictAppend { val, dict } => {
                let v = eval_expr(val, gs)?;
                let mut st = gs.lock().unwrap();
                if *dict >= st.d.len() {
                    st.d.resize(*dict + 1, Vec::new());
                }
                st.d[*dict].push(v);
            }
            Stmt::DictClear(dict) => {
                let mut st = gs.lock().unwrap();
                if *dict < st.d.len() {
                    st.d[*dict].clear();
                }
            }
            Stmt::SaveBin { dict, name } => {
                let base = eval_expr(name, gs)?.display_str();
                let (vec_clone, dir) = {
                    let st = gs.lock().unwrap();
                    let v = st.d.get(*dict).cloned().unwrap_or_default();
                    (v, st.base_dir.clone())
                };
                let file_path = dir.join(format!("nble{}.bin", base));
                let encoded = bincode::serialize(&vec_clone).map_err(|e| e.to_string())?;
                fs::write(&file_path, encoded)
                    .map_err(|e| format!("Failed saving '{}': {}", file_path.display(), e))?;
            }
            Stmt::LoadBin { dict, name } => {
                let base = eval_expr(name, gs)?.display_str();
                let dir = gs.lock().unwrap().base_dir.clone();
                let file_path = dir.join(format!("nble{}.bin", base));
                let bytes = fs::read(&file_path)
                    .map_err(|e| format!("Failed loading '{}': {}", file_path.display(), e))?;
                let decoded: Vec<Value> = bincode::deserialize(&bytes).map_err(|e| e.to_string())?;
                let mut st = gs.lock().unwrap();
                if *dict >= st.d.len() {
                    st.d.resize(*dict + 1, Vec::new());
                }
                st.d[*dict] = decoded;
            }
            Stmt::AngleMode(mode) => {
                let mut st = gs.lock().unwrap();
                st.angle_radians = mode == "radians";
            }
            Stmt::FuncDef { target, def } => match target {
                FuncTarget::GlobalNum(idx) => {
                    gs.lock().unwrap().f.insert(*idx, def.clone());
                }
                FuncTarget::GlobalNamed(name) => {
                    gs.lock().unwrap().f_named.insert(name.clone(), def.clone());
                }
                FuncTarget::LocalNum(idx) => LOCAL_ENV.with(|env| {
                    env.borrow_mut().fl.insert(*idx, def.clone());
                }),
                FuncTarget::LocalNamed(name) => LOCAL_ENV.with(|env| {
                    env.borrow_mut().fl_named.insert(name.clone(), def.clone());
                }),
            },
            Stmt::FuncExec { target, arg } => {
                let arg_val = match arg {
                    Some(a) => Some(eval_expr(a, gs)?),
                    None => None,
                };
                invoke_func(target, arg_val, gs)?;
            }
            Stmt::IfChain { branches, else_body } => {
                let mut matched = false;
                for (l, op, r, body) in branches {
                    if eval_cmp(l, op, r, gs)? {
                        exec_stmts(body, gs, out)?;
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    if let Some(eb) = else_body {
                        exec_stmts(eb, gs, out)?;
                    }
                }
            }
            Stmt::WhileLoop { left, op, right, body } => {
                while eval_cmp(left, op, right, gs)? {
                    let cancelled = LOCAL_ENV.with(|env| {
                        env.borrow()
                            .cancel_flag
                            .as_ref()
                            .map_or(false, |f| f.load(Ordering::Relaxed))
                    });
                    if cancelled {
                        break;
                    }
                    exec_stmts(body, gs, out)?;
                }
            }
            Stmt::IntLoop { idx, op, limit, body } => {
                let delta = match body.first() {
                    Some(Stmt::AddConstInt { delta, .. }) => *delta,
                    _ => 1,
                };
                let mut st = gs.lock().unwrap();
                if *idx >= st.v.len() {
                    st.v.resize(*idx + 1, Value::Int(0));
                }
                let mut cur = st.v[*idx].as_i64().unwrap_or(0);
                while match op {
                    CmpOp::Eq => cur == *limit,
                    CmpOp::NotEq => cur != *limit,
                    CmpOp::Gt => cur > *limit,
                    CmpOp::Lt => cur < *limit,
                    CmpOp::Ge => cur >= *limit,
                    CmpOp::Le => cur <= *limit,
                } {
                    cur = cur.wrapping_add(delta);
                }
                st.v[*idx] = Value::Int(cur);
            }
            Stmt::CountLoop { count, body } => {
                let n = eval_expr(count, gs)?.as_i64()?;
                for _ in 0..n {
                    let cancelled = LOCAL_ENV.with(|env| {
                        env.borrow()
                            .cancel_flag
                            .as_ref()
                            .map_or(false, |f| f.load(Ordering::Relaxed))
                    });
                    if cancelled {
                        break;
                    }
                    exec_stmts(body, gs, out)?;
                }
            }
            Stmt::TryCatch { try_body, catch_body } => {
                if let Err(err_msg) = exec_stmts(try_body, gs, out) {
                    write_var(&VarTarget::GlobalNum(0), Value::Str(err_msg), gs);
                    exec_stmts(catch_body, gs, out)?;
                }
            }
            Stmt::SpawnThread { id, body } => {
                out.flush().ok();
                let id_str = eval_expr(id, gs)?.display_str();
                let flag = Arc::new(AtomicBool::new(false));
                {
                    let mut reg = thread_reg().lock().unwrap();
                    reg.cancel_flags.insert(id_str, flag.clone());
                }
                let gs_clone = Arc::clone(gs);
                let body_clone = body.clone();
                let handle = thread::spawn(move || {
                    LOCAL_ENV.with(|env| {
                        *env.borrow_mut() = LocalEnv::new(Some(flag));
                    });
                    let mut thread_out = BufWriter::with_capacity(65536, io::stdout());
                    let _ = exec_stmts(&body_clone, &gs_clone, &mut thread_out);
                    thread_out.flush().ok();
                });
                thread_reg().lock().unwrap().handles.push(handle);
            }
            Stmt::EndThread(id) => {
                let id_str = eval_expr(id, gs)?.display_str();
                if let Some(flag) = thread_reg().lock().unwrap().cancel_flags.get(&id_str) {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            Stmt::TailThread(id) => {
                out.flush().ok();
                let id_str = eval_expr(id, gs)?.display_str();
                let remaining: Vec<Stmt> = stmts[i + 1..].to_vec();
                let flag = Arc::new(AtomicBool::new(false));
                {
                    let mut reg = thread_reg().lock().unwrap();
                    reg.cancel_flags.insert(id_str, flag.clone());
                }
                let gs_clone = Arc::clone(gs);
                let handle = thread::spawn(move || {
                    LOCAL_ENV.with(|env| {
                        *env.borrow_mut() = LocalEnv::new(Some(flag));
                    });
                    let mut thread_out = BufWriter::with_capacity(65536, io::stdout());
                    let _ = exec_stmts(&remaining, &gs_clone, &mut thread_out);
                    thread_out.flush().ok();
                });
                thread_reg().lock().unwrap().handles.push(handle);
                return Ok(());
            }
        }
        i += 1;
    }
    Ok(())
}

// ============================================================================
// SHELL & CLI ENTRYPOINT
// ============================================================================

fn run_file(path_str: &str, gs: Option<Arc<Mutex<GlobalState>>>) -> Result<(), String> {
    let path = Path::new(path_str.trim().trim_matches('"'));
    let code = fs::read_to_string(path)
        .map_err(|e| format!("Could not read '{}': {}", path.display(), e))?;
    let base_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let state = gs.unwrap_or_else(|| Arc::new(Mutex::new(GlobalState::new(base_dir.clone()))));
    {
        let mut st = state.lock().unwrap();
        st.base_dir = base_dir;
        if let Ok(canon) = fs::canonicalize(path) {
            st.imported_files.insert(canon);
        }
    }

    let stmts = parse_block(&code)?;
    let mut out = BufWriter::with_capacity(65536, io::stdout());
    exec_stmts(&stmts, &state, &mut out)?;
    out.flush().ok();
    join_background_threads();
    Ok(())
}

fn handle_build_cmd(rest: &str) -> Result<(), String> {
    let first_tilde = rest.find('~').ok_or("Usage: build <Name> ~<code>~ @<Directory>")?;
    let name = rest[..first_tilde].trim();
    if name.is_empty() {
        return Err("Missing script name in build command".to_string());
    }
    let after_first = &rest[first_tilde + 1..];
    let second_tilde = after_first.rfind('~').ok_or("Missing closing '~' in build command")?;
    let code = &after_first[..second_tilde];

    // Validate syntax BEFORE touching the filesystem
    let _ = parse_block(code).map_err(|e| format!("Build aborted (syntax invalid) -> {}", e))?;

    let after_second = after_first[second_tilde + 1..].trim();
    let dir_str = after_second
        .strip_prefix('@')
        .map(|s| s.trim().trim_matches('"'))
        .unwrap_or(".");

    let dir = Path::new(dir_str);
    fs::create_dir_all(dir).map_err(|e| format!("Failed creating directory '{}': {}", dir.display(), e))?;
    let file_name = if name.ends_with(".nble") {
        name.to_string()
    } else {
        format!("{}.nble", name)
    };
    let full_path = dir.join(file_name);
    fs::write(&full_path, code).map_err(|e| format!("Failed writing '{}': {}", full_path.display(), e))?;
    println!("Validated & Built {}", full_path.display());
    Ok(())
}

fn handle_condense_cmd(rest: &str) -> Result<(), String> {
    let path = Path::new(rest.trim().trim_matches('"'));
    let code = fs::read_to_string(path)
        .map_err(|e| format!("Could not read '{}': {}", path.display(), e))?;
    let _ = parse_block(&code)?;
    let condensed = condense_source(&code);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("script");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let out_path = parent.join(format!("{}condensed.nble", stem));
    fs::write(&out_path, condensed)
        .map_err(|e| format!("Failed writing '{}': {}", out_path.display(), e))?;
    println!("Condensed -> {}", out_path.display());
    Ok(())
}

fn handle_valid_cmd(rest: &str) -> Result<(), String> {
    let path = Path::new(rest.trim().trim_matches('"'));
    let code = fs::read_to_string(path)
        .map_err(|e| format!("Could not read '{}': {}", path.display(), e))?;
    let stmts = parse_block(&code)?;
    println!("Valid NBLE 1.0 script ({} top-level statements).", stmts.len());
    Ok(())
}

fn print_help() {
    println!("NBLE 1.0 Interactive Shell Commands:");
    println!("  run <path.nble>                 Execute an NBLE script file");
    println!("  valid <path.nble>               Validate syntax of a .nble script");
    println!("  build <Name> ~code~ @<Dir>      Validate & create a .nble script");
    println!("  condense <path.nble>            Validate & minify into *condensed.nble");
    println!("  reset                           Clear all variables, functions, and dicts");
    println!("  help                            Show this help message");
    println!("  exit / quit                     Close the NBLE 1.0 shell");
    println!("  <inline NBLE statements>        Execute NBLE code directly in the shell");
}

fn interactive_shell() {
    set_console_title("NBLE 1.0 Interactive Shell");
    install_ctrl_c_handler();

    println!("==========================================");
    println!("   NBLE 1.0 Interactive Shell Ready");
    println!("   Type 'help' for commands or 'exit'");
    println!("==========================================");

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut gs = Arc::new(Mutex::new(GlobalState::new(cwd.clone())));
    let mut buffer = String::new();
    let mut brace_depth: i32 = 0;

    loop {
        if brace_depth > 0 {
            print!("...     ");
        } else {
            print!("nble1.0> ");
        }
        io::stdout().flush().ok();

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {}", e);
                continue;
            }
        }

        let trimmed = line.trim();
        if brace_depth == 0 {
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "exit" || trimmed == "quit" {
                cancel_all_threads();
                break;
            }
            if trimmed == "help" {
                print_help();
                continue;
            }
            if trimmed == "reset" {
                cancel_all_threads();
                join_background_threads();
                gs = Arc::new(Mutex::new(GlobalState::new(cwd.clone())));
                println!("Session memory cleared.");
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("run ") {
                if let Err(e) = run_file(rest, Some(Arc::clone(&gs))) {
                    eprintln!("Error: {}", e);
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("valid ") {
                if let Err(e) = handle_valid_cmd(rest) {
                    eprintln!("Error: {}", e);
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("build ") {
                if let Err(e) = handle_build_cmd(rest) {
                    eprintln!("Error: {}", e);
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("condense ") {
                if let Err(e) = handle_condense_cmd(rest) {
                    eprintln!("Error: {}", e);
                }
                continue;
            }
        }

        let mut in_str = false;
        let bytes = line.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            if in_str {
                if bytes[idx] == b'\\' && idx + 1 < bytes.len() {
                    idx += 2;
                    continue;
                }
                if bytes[idx] == b'"' {
                    in_str = false;
                }
                idx += 1;
                continue;
            }
            if bytes[idx] == b'"' {
                in_str = true;
            } else if bytes[idx] == b'{' {
                brace_depth += 1;
            } else if bytes[idx] == b'}' {
                brace_depth = (brace_depth - 1).max(0);
            }
            idx += 1;
        }

        buffer.push_str(&line);
        if brace_depth == 0 {
            let code = std::mem::take(&mut buffer);
            match parse_block(&code) {
                Ok(stmts) => {
                    let mut out = BufWriter::with_capacity(65536, io::stdout());
                    if let Err(e) = exec_stmts(&stmts, &gs, &mut out) {
                        eprintln!("Runtime Error: {}", e);
                    }
                    out.flush().ok();
                    join_background_threads();
                }
                Err(e) => eprintln!("Parse Error: {}", e),
            }
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() == 1 {
        #[cfg(windows)]
        {
            if let Ok(exe) = env::current_exe() {
                if Command::new(exe)
                    .arg("--shell")
                    .creation_flags(CREATE_NEW_CONSOLE)
                    .spawn()
                    .is_ok()
                {
                    return;
                }
            }
        }
        interactive_shell();
        return;
    }

    if args[1] == "--shell" {
        interactive_shell();
        return;
    }

    if args[1] == "valid" && args.len() >= 3 {
        if let Err(e) = handle_valid_cmd(&args[2]) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if args[1] == "condense" && args.len() >= 3 {
        if let Err(e) = handle_condense_cmd(&args[2]) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if args[1] == "build" && args.len() >= 3 {
        let joined = args[2..].join(" ");
        if let Err(e) = handle_build_cmd(&joined) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let script_arg = if args[1] == "run" && args.len() >= 3 {
        &args[2]
    } else {
        &args[1]
    };

    if let Err(e) = run_file(script_arg, None) {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}