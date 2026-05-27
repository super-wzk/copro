use copro_core::tool::Tool;
use schemars::JsonSchema;
use serde::Deserialize;

// ---- Calculator ------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalculatorInput {
    /// A mathematical expression to evaluate, e.g. "2 + 3 * 4".
    pub expression: String,
}

#[derive(Debug)]
pub struct Calculator;

impl Tool for Calculator {
    type Input = CalculatorInput;
    type Output = f64;

    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Evaluate a simple arithmetic expression. Supports +, -, *, /, and parentheses."
    }

    fn call(&self, input: Self::Input) -> std::result::Result<Self::Output, String> {
        evaluate_expression(&input.expression).map_err(|e| e.to_string())
    }
}

fn evaluate_expression(expr: &str) -> Result<f64, String> {
    // Whitelist: only digits, operators, parens, spaces, decimal points
    let cleaned: String = expr
        .chars()
        .filter(|c| c.is_ascii_digit() || "+-*/(). ".contains(*c))
        .collect();

    if cleaned.is_empty() {
        return Err("empty expression".into());
    }

    // Simple recursive descent parser for +, -, *, /
    let tokens = tokenize(&cleaned);
    let mut pos = 0;
    let result = parse_expr(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return Err(format!("unexpected token at position {pos}"));
    }
    Ok(result)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Add,
    Sub,
    Mul,
    Div,
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num_buf.push(ch);
        } else {
            if !num_buf.is_empty() {
                tokens.push(Token::Num(num_buf.parse().unwrap_or(0.0)));
                num_buf.clear();
            }
            match ch {
                '+' => tokens.push(Token::Add),
                '-' => tokens.push(Token::Sub),
                '*' => tokens.push(Token::Mul),
                '/' => tokens.push(Token::Div),
                '(' => tokens.push(Token::LParen),
                ')' => tokens.push(Token::RParen),
                _ => {}
            }
        }
    }
    if !num_buf.is_empty() {
        tokens.push(Token::Num(num_buf.parse().unwrap_or(0.0)));
    }
    tokens
}

// expr   = term (('+' | '-') term)*
// term   = factor (('*' | '/') factor)*
// factor = Num | '(' expr ')' | '-' factor

fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut lhs = parse_term(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            Token::Add => {
                *pos += 1;
                lhs += parse_term(tokens, pos)?;
            }
            Token::Sub => {
                *pos += 1;
                lhs -= parse_term(tokens, pos)?;
            }
            _ => break,
        }
    }
    Ok(lhs)
}

fn parse_term(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut lhs = parse_factor(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            Token::Mul => {
                *pos += 1;
                lhs *= parse_factor(tokens, pos)?;
            }
            Token::Div => {
                *pos += 1;
                let rhs = parse_factor(tokens, pos)?;
                if rhs == 0.0 {
                    return Err("division by zero".into());
                }
                lhs /= rhs;
            }
            _ => break,
        }
    }
    Ok(lhs)
}

fn parse_factor(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    if *pos >= tokens.len() {
        return Err("unexpected end of expression".into());
    }
    match tokens[*pos] {
        Token::Num(n) => {
            *pos += 1;
            Ok(n)
        }
        Token::LParen => {
            *pos += 1;
            let result = parse_expr(tokens, pos)?;
            if *pos >= tokens.len() || tokens[*pos] != Token::RParen {
                return Err("missing closing parenthesis".into());
            }
            *pos += 1;
            Ok(result)
        }
        Token::Sub => {
            *pos += 1;
            Ok(-parse_factor(tokens, pos)?)
        }
        _ => Err(format!("unexpected token: {:?}", tokens[*pos])),
    }
}

// ---- DateTime --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DateTimeInput {
    /// Timezone offset in hours from UTC, e.g. 8 for Asia/Shanghai.
    #[serde(default)]
    pub timezone_offset: i32,
}

#[derive(Debug)]
pub struct DateTimeTool;

impl Tool for DateTimeTool {
    type Input = DateTimeInput;
    type Output = String;

    fn name(&self) -> &str {
        "datetime"
    }

    fn description(&self) -> &str {
        "Get the current date and time, optionally adjusted by a timezone offset."
    }

    fn call(&self, input: Self::Input) -> Result<Self::Output, String> {
        let now = std::time::SystemTime::now();
        let total_secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs() as i64;
        let adjusted = total_secs + input.timezone_offset as i64 * 3600;
        let days = adjusted / 86400;
        let secs_in_day = adjusted % 86400;
        let hours = secs_in_day / 3600;
        let minutes = (secs_in_day % 3600) / 60;
        let seconds = secs_in_day % 60;

        // Simple date calculation from Unix epoch
        let (year, month, day) = epoch_to_date(days);
        let offset_label = if input.timezone_offset >= 0 {
            format!("+{}", input.timezone_offset)
        } else {
            format!("{}", input.timezone_offset)
        };

        Ok(format!(
            "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02} UTC{offset_label}",
        ))
    }
}

fn epoch_to_date(days: i64) -> (i64, u32, u32) {
    // Algorithm from Howard Hinnant
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculator_basic_ops() {
        assert!((evaluate_expression("2 + 3 * 4").unwrap() - 14.0).abs() < 0.001);
        assert!((evaluate_expression("(2 + 3) * 4").unwrap() - 20.0).abs() < 0.001);
        assert!((evaluate_expression("-5 + 3").unwrap() - (-2.0)).abs() < 0.001);
        assert!((evaluate_expression("10 / 3").unwrap() - 3.333).abs() < 0.001);
    }
}
