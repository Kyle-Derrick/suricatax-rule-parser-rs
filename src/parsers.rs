// SPDX-FileCopyrightText: (C) 2021 Jason Ish <jason@codemonkey.net>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core parsers for basic and common types, as well as parsers for
//! trivial keywords.

use std::{fmt, str::FromStr};

use nom::{
    branch::alt,
    bytes::complete::{escaped_transform, is_not, tag, take_until, take_while},
    character::complete::{alphanumeric1, multispace0, none_of, one_of},
    combinator::{map, opt, rest},
    multi::separated_list0,
    sequence::{delimited, preceded, terminated, tuple},
    IResult,
};
use num_traits::Num;

use crate::{options::*, types::*};

pub(crate) mod byte_jump;
pub(crate) mod byte_test;

static WHITESPACE: &str = " \t\r\n";

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum ErrorKind {
    UnterminatedArray,
    UnterminatedRuleOptionValue,
    BadNumber,
    UnexpectedCharacter,
    Invalid,
    Other(&'static str),
    Nom(nom::error::ErrorKind),
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ErrorKind::UnterminatedArray => write!(f, "unterminated array"),
            ErrorKind::UnterminatedRuleOptionValue => write!(f, "unterminated rule option value"),
            ErrorKind::BadNumber => write!(f, "bad number"),
            ErrorKind::UnexpectedCharacter => write!(f, "unexpected character"),
            ErrorKind::Other(s) => write!(f, "{}", s),
            ErrorKind::Nom(kind) => write!(f, "nom error: {}", kind.description()),
            ErrorKind::Invalid => write!(f, "invalid"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct ParseError<I> {
    pub kind: ErrorKind,
    pub input: I,
}

/// Converts a nom error into a ParseError.
///
/// Allows parsers to return a normal Result when not used as part of
/// a combinator.
impl<'a> From<nom::Err<ParseError<&'a str>>> for ParseError<&'a str> {
    fn from(err: nom::Err<ParseError<&'a str>>) -> Self {
        match err {
            nom::Err::Error(err) => err,
            nom::Err::Failure(err) => err,
            nom::Err::Incomplete(_) => unreachable!(),
        }
    }
}

impl<I> nom::error::ParseError<I> for ParseError<I> {
    fn from_error_kind(input: I, kind: nom::error::ErrorKind) -> Self {
        Self {
            kind: ErrorKind::Nom(kind),
            input,
        }
    }

    fn append(_: I, _: nom::error::ErrorKind, other: Self) -> Self {
        other
    }
}

/// Get the next sequence of characters up until the next whitespace,
/// ignoring any leading whitespace.
pub(crate) fn take_until_whitespace(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    preceded(multispace0, nom::bytes::complete::is_not(WHITESPACE))(input)
}

/// Parse a tag ignoring any leading whitespace.
///
/// Useful for parsing an expected separator or keyword.
pub(crate) fn parse_tag(sep: &str) -> impl Fn(&str) -> IResult<&str, &str, ParseError<&str>> + '_ {
    move |input| preceded(multispace0, tag(sep))(input)
}

/// Parse the next token ignoring leading whitespace.
///
/// A token is the next sequence of chars until a terminating character. Leading whitespace
/// is ignored.
pub(crate) fn parse_token(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    let terminators = "\n\r\t,;: ";
    preceded(multispace0, is_not(terminators))(input)
}

/// Parse a quote string as often seen in Suricata rules.
///
/// This handles escaped quotes and semicolons (however semicolons do not need
/// to be escaped like most parsers enforce).
///
/// The input string must start with a quote and will parse up to the next
/// unescaped quote.
///
/// The return value is a String with escapes removed and no leading or trailing
/// double quotes.
fn parse_quoted_string(input: &str) -> IResult<&str, String, ParseError<&str>> {
    let escaped_parser = escaped_transform(none_of("\\\""), '\\', one_of("\"\\;\\:"));
    let empty = map(tag(""), |s: &str| s.to_string());
    let escaped_or_empty = alt((escaped_parser, empty));
    delimited(tag("\""), escaped_or_empty, tag("\""))(input)
}

/// Checks if the last element of a vector matches a given value
///
/// # Arguments
/// - `$stack`: Vector to check (must implement `.last()`)
/// - `$check_value`: Value to compare against the last element
/// - `$if_eq`: Token tree to execute if last element matches
/// - `$if_ne`: Token tree to execute if no match or vector is empty
///
/// # Return value
/// Based on the comparison result, return the execution result of the `$if_eq` or `$if_ne` code block
macro_rules! stack_check_last {
    ($stack: expr, $check_value: expr, $if_eq: tt, $if_ne: tt) => {
        {
            let mut __last_eq_value__ = false;
            if let Some(val) = $stack.last() {
                if $check_value == *val {
                    __last_eq_value__ = true;
                }
            }
            if __last_eq_value__ {
                $if_eq
            } else {
                $if_ne
            }
        }
    };
}

/// Processes accumulated tokens and adds them to the parser stack
///
/// # Parameters
/// - `input`: Current input string (for error reporting)
/// - `depth`: Current parsing depth level
/// - `depth_when_not`: Stack tracking special negation depths
/// - `token`: Accumulated string token (cleared after processing)
/// - `stack`: Main parser stack (nested array structure)
///
/// # Returns
/// - `Ok(((), ()))` on successful processing
/// - `Err(ParseError)` if:
///    - Token exists but stack is empty (unterminated array)
#[inline]
fn check_token_and_add<'a>(input: &'a str,
                           depth: i32,
                           depth_when_not: &mut Vec<i32>,
                           token: &mut String,
                           stack: &mut Vec<Vec<ArrayElement>>) -> IResult<(), (), ParseError<&'a str>> {
    if !token.is_empty() {
        let token_str = token.trim_end();
        if let Some(top) = stack.last_mut() {
            // If stack has top level:
            //    - Uses `stack_check_last!` to determine element type:
            //      * Match: Creates NOT element and pops depth_when_not
            //      * No match: Creates normal String element
            //    - Clears token buffer
            top.push(stack_check_last!(depth_when_not, depth, {
                        depth_when_not.pop();
                         ArrayElement::not_string(token_str.to_string())
                    }, {
                         ArrayElement::String(token_str.to_string())
                    }));
            token.clear();
        } else {
            return Err(nom::Err::Error(ParseError {
                kind: ErrorKind::UnterminatedArray,
                input,
            }));
        }
    }
    Ok(((), ()))
}

pub(crate) fn parse_array(input: &str) -> IResult<&str, Vec<ArrayElement>, ParseError<&str>> {
    // Use a stack to avoid recursion. Should probably still set a
    // size bound on it.
    let mut stack: Vec<Vec<ArrayElement>> = vec![Vec::new()];
    let mut token = String::new();
    let mut depth = 0;
    let mut offset = 0;

    let mut neg = false;

    let mut input = input.trim_start();
    if input.starts_with('!') {
        neg = true;
        input = &input[1..];
    }

    // We might not always have an array, if not, parse a scalar and
    // return it as an array.
    if !input.starts_with('[') {
        let (input, scalar) = preceded(multispace0, is_not("\n\r\t "))(input)?;
        let element = if neg {
            ArrayElement::not_string(scalar.to_string())
        } else {
            ArrayElement::String(scalar.to_string())
        };
        return Ok((input, vec![element]));
    }

    let mut depth_when_not: Vec<i32> = Vec::with_capacity(5);
    for c in input.chars() {
        offset += c.len_utf8();
        match c {
            '[' => {
                depth += 1;
                stack.push(Vec::new());
            }
            ']' => {
                check_token_and_add(input, depth, &mut depth_when_not, &mut token, &mut stack)?;
                let last = stack.pop().ok_or(nom::Err::Error(ParseError {
                    kind: ErrorKind::UnterminatedArray,
                    input,
                }))?;
                if let Some(top) = stack.last_mut() {
                    top.push(stack_check_last!(depth_when_not, depth - 1, {
                        depth_when_not.pop();
                        ArrayElement::not_array(last)
                    }, {
                        ArrayElement::Array(last)
                    }));
                } else {
                    return Err(nom::Err::Error(ParseError {
                        kind: ErrorKind::UnterminatedArray,
                        input,
                    }));
                }

                depth -= 1;

                if depth == 0 {
                    break;
                }
            }
            ',' => {
                check_token_and_add(input, depth, &mut depth_when_not, &mut token, &mut stack)?;
            }
            ' ' | '\t' if token.is_empty() => {
                continue
            }
            '!' if token.is_empty() => {
                depth_when_not.push(depth);
            }
            _ => token.push(c),
        }
    }

    if !token.is_empty() {
        if let Some(top) = stack.last_mut() {
            top.push(ArrayElement::String(token.clone()));
        } else {
            return Err(nom::Err::Error(ParseError {
                kind: ErrorKind::UnterminatedArray,
                input,
            }));
        }
    }

    if depth > 0 {
        return Err(nom::Err::Error(ParseError {
            kind: ErrorKind::UnterminatedArray,
            input,
        }));
    }

    // Double unwrap as we used a stack to avoid recursion.
    if let Some(mut stack) = stack.pop() {
        if let Some(ArrayElement::Array(stack)) = stack.pop() {
            let stack = if neg {
                vec![ArrayElement::not_array(stack)]
            } else {
                stack
            };
            return Ok((&input[offset..], stack));
        }
    }

    Err(nom::Err::Error(ParseError {
        kind: ErrorKind::UnterminatedArray,
        input,
    }))
}

/// Scan an array returning a String of the array contents.
pub(crate) fn scan_array(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    let input = input.trim_start();

    // We might not always have an array, if not, parse a scalar.
    if !input.starts_with('[') {
        let (input, scalar) = preceded(multispace0, is_not("\n\r\t "))(input)?;
        return Ok((input, scalar));
    }

    let mut depth = 0;
    let mut offset = 0;

    for c in input.chars() {
        offset += c.len_utf8();
        match c {
            '[' => {
                depth += 1;
            }
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }

    Ok((&input[offset..], &input[0..offset]))
}

/// Parse the value for an option.
///
/// This parser expects the input to be the first character after the ':'
/// following an option name. It will return all the characters up to but not
/// including the option terminator ';', handling all escaped occurrences of the
/// option terminator.
///
/// The remaining input returned does not contain the option terminator.
pub(crate) fn parse_option_value(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    let mut escaped = false;
    let mut end = 0;
    let mut terminated = false;

    // First jump over any leading whitespace.
    let (input, _) = multispace0(input)?;

    for c in input.chars() {
        end += c.len_utf8();
        if c == '\\' {
            escaped = true;
        } else if escaped {
            escaped = false;
        } else if c == ';' {
            terminated = true;
            break;
        }
    }

    if !terminated {
        Err(nom::Err::Error(ParseError {
            kind: ErrorKind::UnterminatedRuleOptionValue,
            input,
        }))
    } else {
        Ok((&input[(end - 1) + 1..], &input[0..(end - 1)]))
    }
}

pub(crate) fn start_of_options(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    preceded(multispace0, tag("("))(input)
}

pub(crate) fn end_of_options(input: &str) -> IResult<&str, &str> {
    preceded(multispace0, tag(")"))(input)
}

pub(crate) fn option_name(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    preceded(multispace0, nom::bytes::complete::is_not(";:"))(input)
}

pub(crate) fn options_separator(input: &str) -> IResult<&str, char, ParseError<&str>> {
    preceded(multispace0, nom::character::complete::one_of(";:"))(input)
}

pub(crate) fn parse_direction(input: &str) -> IResult<&str, Direction, ParseError<&str>> {
    let parse_single = |input| -> IResult<&str, Direction, ParseError<&str>> {
        let (input, _) = tag("->")(input)?;
        Ok((input, Direction::Single))
    };

    let parse_both = |input| -> IResult<&str, Direction, ParseError<&str>> {
        let (input, _) = tag("<>")(input)?;
        Ok((input, Direction::Both))
    };

    let (rem, direction) = alt((parse_single, parse_both))(input).map_err(|_| {
        nom::Err::Error(ParseError {
            kind: ErrorKind::Invalid,
            input,
        })
    })?;
    Ok((rem, direction))
}

/// Parser a number of type T.
pub(crate) fn parse_number<T: FromStr + Num>(input: &str) -> IResult<&str, T, ParseError<&str>> {
    let (rem, token) = parse_token(input)?;
    let number = if token.starts_with("0x") || token.starts_with("0X") {
        T::from_str_radix(&token[2..], 16).map_err(|_| {
            nom::Err::Error(ParseError {
                kind: ErrorKind::BadNumber,
                input: token,
            })
        })?
    } else {
        token.parse::<T>().map_err(|_| {
            nom::Err::Error(ParseError {
                kind: ErrorKind::BadNumber,
                input: token,
            })
        })?
    };
    Ok((rem, number))
}

pub(crate) fn parse_number_or_reference<T: FromStr + Num>(
    input: &str,
) -> IResult<&str, NumberOrReference<T>, ParseError<&str>> {
    if let Ok((input, number)) = parse_number::<T>(input) {
        Ok((input, NumberOrReference::Number(number)))
    } else {
        let (input, name) = parse_token(input)?;
        Ok((input, NumberOrReference::Name(name.to_string())))
    }
}

/// Parse an end quote. Probably not the best name for thie parser but it parses up to and
/// including a quote that is only prefixed by optional whitespace.
fn parse_end_quote(input: &str) -> IResult<&str, &str, ParseError<&str>> {
    preceded(multispace0, tag("\""))(input)
}

/// Parse the metadata into a list of the comma separated values.
pub(crate) fn metadata(input: &str) -> IResult<&str, Vec<String>, ParseError<&str>> {
    let sep = terminated(multispace0, preceded(multispace0, tag(",")));
    let (input, parts) = separated_list0(
        sep,
        preceded(multispace0, take_while(|c| c != ',' && c != ';')),
    )(input)?;
    let parts: Vec<String> = parts.iter().map(|p| p.trim().to_string()).collect();
    Ok((input, parts))
}

pub(crate) fn parse_content(input: &str) -> IResult<&str, Content, ParseError<&str>> {
    let (input, negate) = preceded(multispace0, opt(tag("!")))(input)?;
    let (input, pattern) = parse_quoted_string(input)?;
    Ok((
        input,
        Content {
            pattern,
            negated: negate.is_some(),
        },
    ))
}

pub(crate) fn parse_flow(input: &str) -> IResult<&str, Flow, ParseError<&str>> {
    let sep = terminated(multispace0, preceded(multispace0, tag(",")));
    let (input, parts) = separated_list0(
        sep,
        preceded(multispace0, take_while(|c| c != ',' && c != ';')),
    )(input)?;

    let mut flow = Flow::default();

    for part in parts {
        match part.trim() {
            "not_established" => flow.not_established = true,
            "established" => flow.established = true,
            "from_client" => flow.from_client = true,
            "from_server" => flow.from_server = true,
            "no_frag" => flow.no_frag = true,
            "no_stream" => flow.no_stream = true,
            "only_frag" => flow.only_frag = true,
            "only_stream" => flow.only_stream = true,
            "stateless" => flow.stateless = true,
            "to_client" => flow.to_client = true,
            "to_server" => flow.to_server = true,
            _ => {
                return Err(nom::Err::Error(ParseError {
                    kind: ErrorKind::Other("unexpected flow option"),
                    input: part,
                }));
            }
        }
    }

    Ok((input, flow))
}

pub(crate) fn parse_pcre(input: &str) -> IResult<&str, Pcre, ParseError<&str>> {
    let (input, negate) = opt(tag("!"))(input)?;
    let (input, _open_quote) = preceded(multispace0, tag("\""))(input)?;
    let (input, _open_pcre) = tag("/")(input)?;
    let pattern_end = input.rfind('/').ok_or({
        nom::Err::Error(ParseError {
            kind: ErrorKind::Other("pcre: no terminating /"),
            input,
        })
    })?;
    let pattern = &input[0..pattern_end];
    let input = &input[pattern_end..];
    let (input, _close_re) = tag("/")(input)?;

    // Return what we have if we're at the end of the quoted section.
    if let Ok((input, _)) = parse_end_quote(input) {
        let pcre = Pcre {
            negate: negate.is_some(),
            pattern: pattern.to_string(),
            modifiers: "".to_string(),
            vars: vec![],
        };
        return Ok((input, pcre));
    }

    // Now parse the modifiers.
    let (input, modifiers) = alphanumeric1(input)?;

    // There might also be some variable captures.
    let parse_start_of_vars = preceded(multispace0, tag(","));
    let parse_vars = preceded(parse_start_of_vars, take_until("\""));
    let (input, vars) = opt(parse_vars)(input)?;
    let (input, _) = parse_end_quote(input)?;

    let vars: Vec<String> = if let Some(vars) = vars {
        vars.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        vec![]
    };

    let pcre = Pcre {
        negate: negate.is_some(),
        pattern: pattern.to_string(),
        modifiers: modifiers.to_string(),
        vars,
    };
    Ok((input, pcre))
}

pub(crate) fn parse_isdataat(input: &str) -> IResult<&str, IsDataAt, ParseError<&str>> {
    // Look for a possible negation flag.
    let (input, negate) = preceded(multispace0, opt(tag("!")))(input)?;
    let (input, position) = parse_token(input)?;
    let position = if let Ok((_, number)) = parse_number::<u64>(position) {
        IsDataAtPosition::Position(number)
    } else {
        IsDataAtPosition::Identifier(position.to_string())
    };
    let mut relative = false;
    let mut rawbytes = false;

    for option in input.split(',').map(|s| s.trim()) {
        match option {
            "relative" => {
                relative = true;
            }
            "rawbytes" => {
                rawbytes = true;
            }
            "" => {}
            _ => {
                return Err(nom::Err::Error(ParseError {
                    kind: ErrorKind::Other("invalid option"),
                    input: option,
                }));
            }
        }
    }
    Ok((
        "",
        IsDataAt {
            negate: negate.is_some(),
            position,
            relative,
            rawbytes,
        },
    ))
}

pub(crate) fn parse_flowbits(input: &str) -> IResult<&str, Flowbits, ParseError<&str>> {
    let command_parser = preceded(multispace0, alphanumeric1);
    let name_parser = preceded(tag(","), preceded(multispace0, rest));
    let (input, (command, names)) = tuple((command_parser, opt(name_parser)))(input)?;

    let command = match command {
        "noalert" => FlowbitCommand::NoAlert,
        "set" => FlowbitCommand::Set,
        "unset" => FlowbitCommand::Unset,
        "toggle" => FlowbitCommand::Toggle,
        "isnotset" => FlowbitCommand::IsNotSet,
        "isset" => FlowbitCommand::IsSet,
        _ => {
            return Err(nom::Err::Error(ParseError {
                kind: ErrorKind::Other("invalid flowbits command"),
                input: command,
            }));
        }
    };

    match command {
        FlowbitCommand::IsNotSet
        | FlowbitCommand::Unset
        | FlowbitCommand::Toggle
        | FlowbitCommand::IsSet
        | FlowbitCommand::Set => {
            let names = names
                .ok_or({
                    nom::Err::Error(ParseError {
                        kind: ErrorKind::Other("argument required"),
                        input,
                    })
                })?
                .split('|')
                .map(|s| s.trim().to_string())
                .collect();
            Ok((input, Flowbits { command, names }))
        }
        FlowbitCommand::NoAlert => {
            if names.is_some() {
                Err(nom::Err::Error(ParseError {
                    kind: ErrorKind::Other("noalert does not take arguments"),
                    input,
                }))
            } else {
                Ok((
                    input,
                    Flowbits {
                        command,
                        names: vec![],
                    },
                ))
            }
        }
    }
}

pub(crate) fn parse_reference(input: &str) -> IResult<&str, Reference, ParseError<&str>> {
    let (input, scheme) = take_until(",")(input)?;
    let (input, _) = tag(",")(input)?;
    let (input, reference) = preceded(multispace0, rest)(input)?;
    Ok((
        input,
        Reference {
            scheme: scheme.to_string(),
            reference: reference.to_string(),
        },
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_take_until_whitespace() {
        assert_eq!(take_until_whitespace("foo").unwrap(), ("", "foo"));
        assert_eq!(take_until_whitespace("foo bar").unwrap(), (" bar", "foo"));
        assert_eq!(
            take_until_whitespace("   foo bar").unwrap(),
            (" bar", "foo")
        );
        assert_eq!(
            take_until_whitespace("foo\t bar").unwrap(),
            ("\t bar", "foo")
        );
        assert_eq!(
            take_until_whitespace("foo\r bar").unwrap(),
            ("\r bar", "foo")
        );
        assert_eq!(
            take_until_whitespace("foo\n bar").unwrap(),
            ("\n bar", "foo")
        );
    }

    #[test]
    fn test_parse_array() {
        let input = "[a]xxx";
        let (rem, array) = parse_array(input).unwrap();
        assert_eq!(rem, "xxx");
        assert_eq!(array, vec![ArrayElement::String("a".to_string())]);

        let input = "[a,bbb]xxx";
        let (rem, array) = parse_array(input).unwrap();
        assert_eq!(rem, "xxx");
        assert_eq!(
            array,
            vec![
                ArrayElement::String("a".to_string()),
                ArrayElement::String("bbb".to_string())
            ]
        );

        let input = "[a,[bbb,ccc,[xxx]],ddd,[eee,fff]]aaa";
        let (rem, array) = parse_array(input).unwrap();
        assert_eq!(rem, "aaa");
        assert_eq!(
            array,
            vec![
                ArrayElement::String("a".to_string()),
                ArrayElement::Array(vec![
                    ArrayElement::String("bbb".to_string()),
                    ArrayElement::String("ccc".to_string()),
                    ArrayElement::Array(vec![ArrayElement::String("xxx".to_string())]),
                ]),
                ArrayElement::String("ddd".to_string()),
                ArrayElement::Array(vec![
                    ArrayElement::String("eee".to_string()),
                    ArrayElement::String("fff".to_string()),
                ]),
            ]
        );

        let input = "ff02::fb";
        let (_rem, array) = parse_array(input).unwrap();
        assert_eq!(array, vec![ArrayElement::String("ff02::fb".to_string())]);

        assert!(parse_array("[a").is_err());

        assert_eq!(
            parse_array("[asdf,asdf").unwrap_err(),
            nom::Err::Error(ParseError {
                kind: ErrorKind::UnterminatedArray,
                input: "[asdf,asdf"
            })
        );
        assert_eq!(
            parse_array("[[asdf,asdf]").unwrap_err(),
            nom::Err::Error(ParseError {
                kind: ErrorKind::UnterminatedArray,
                input: "[[asdf,asdf]"
            })
        );
    }

    #[test]
    fn test_scan_array() {
        let input = "[a]xxx";
        let (rem, array) = scan_array(input).unwrap();
        assert_eq!(rem, "xxx");
        assert_eq!(array, "[a]");

        let input = "[a,bbb]xxx";
        let (rem, array) = parse_array(input).unwrap();
        assert_eq!(rem, "xxx");
        assert_eq!(
            array,
            vec![
                ArrayElement::String("a".to_string()),
                ArrayElement::String("bbb".to_string())
            ]
        );

        let input = "[a,bbb]xxx";
        let (rem, array) = scan_array(input).unwrap();
        assert_eq!(rem, "xxx");
        assert_eq!(array, "[a,bbb]");

        let input = "[a,[bbb,ccc,[xxx]],ddd,[eee,fff]]aaa";
        let (rem, array) = scan_array(input).unwrap();
        assert_eq!(rem, "aaa");
        assert_eq!(array, "[a,[bbb,ccc,[xxx]],ddd,[eee,fff]]");

        let input = "ff02::fb 8080";
        let (rem, array) = scan_array(input).unwrap();
        assert_eq!(rem, " 8080");
        assert_eq!(array, "ff02::fb");

        // The array scanner does not treat this as an error. Perhaps
        // it should.
        let (rem, array) = scan_array("[a").unwrap();
        assert_eq!(rem, "");
        assert_eq!(array, "[a");
    }

    #[test]
    fn test_parse_option_value() {
        let (rem, value) = parse_option_value("value;").unwrap();
        assert_eq!(rem, "");
        assert_eq!(value, "value");

        let (rem, value) = parse_option_value("   value;").unwrap();
        assert_eq!(rem, "");
        assert_eq!(value, "value");

        let (rem, value) = parse_option_value("   value ;").unwrap();
        assert_eq!(rem, "");
        assert_eq!(value, "value ");

        let (rem, value) = parse_option_value("   value ;next option").unwrap();
        assert_eq!(rem, "next option");
        assert_eq!(value, "value ");
    }

    #[test]
    fn test_parse_direction() {
        assert_eq!(parse_direction("->").unwrap(), ("", Direction::Single));
        assert_eq!(parse_direction("<>").unwrap(), ("", Direction::Both));
        assert!(parse_direction("-").is_err());
        assert!(parse_direction("<-").is_err());
    }

    #[test]
    fn test_parse_quoted_string() {
        let (i, a) = parse_quoted_string(r#""""#).unwrap();
        assert_eq!(i, "");
        assert_eq!(a, "");

        let (i, a) = parse_quoted_string(r#""simple string""#).unwrap();
        assert_eq!(i, "");
        assert_eq!(a, "simple string");

        let (i, a) = parse_quoted_string(r#""with; semicolons.""#).unwrap();
        assert_eq!(i, "");
        assert_eq!(a, "with; semicolons.");

        let (i, a) = parse_quoted_string(r#""with escaped\; semicolons.""#).unwrap();
        assert_eq!(i, "");
        assert_eq!(a, "with escaped; semicolons.");

        let (i, a) =
            parse_quoted_string(r#""with escaped\; semicolons and \" inner quote""#).unwrap();
        assert_eq!(i, "");
        assert_eq!(a, "with escaped; semicolons and \" inner quote");
    }

    #[test]
    fn test_parse_content() {
        let (i, content) = parse_content(r#""|be ef|""#).unwrap();
        assert_eq!(
            content,
            Content {
                pattern: "|be ef|".to_string(),
                negated: false,
            }
        );
        assert_eq!(i, "");

        let (i, content) = parse_content(r#"!"|be ef|""#).unwrap();
        assert_eq!(
            content,
            Content {
                pattern: "|be ef|".to_string(),
                negated: true,
            }
        );
        assert_eq!(i, "");

        // Snort 3 style...
        let (i, content) = parse_content(r#"!"|be ef|", within 5"#).unwrap();
        assert_eq!(
            content,
            Content {
                pattern: "|be ef|".to_string(),
                negated: true,
            }
        );
        assert_eq!(i, ", within 5");

        let (rem, content) = parse_content(r#""/pda_projects.php?offset=http\:""#).unwrap();
        assert_eq!(rem, "");
        assert_eq!(
            content,
            Content {
                negated: false,
                pattern: r#"/pda_projects.php?offset=http:"#.to_string(),
            }
        )
    }

    #[test]
    fn test_parse_pcre() {
        let input0 = r#""/[0-9]{6}/""#;
        let (rem, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(rem, "");
        assert_eq!(
            pcre,
            Pcre {
                negate: false,
                pattern: r#"[0-9]{6}"#.to_string(),
                modifiers: "".to_string(),
                vars: vec![],
            }
        );

        let input0 = r#""/[0-9]{6}/UR""#;
        let (rem, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(rem, "");
        assert_eq!(
            pcre,
            Pcre {
                negate: false,
                pattern: r#"[0-9]{6}"#.to_string(),
                modifiers: "UR".to_string(),
                vars: vec![],
            }
        );

        let input0 = "\"/([^:/$]+)/R,flow:rce_server\"";
        let (_, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(
            pcre,
            Pcre {
                negate: false,
                pattern: r#"([^:/$]+)"#.to_string(),
                modifiers: "R".to_string(),
                vars: vec!["flow:rce_server".to_string()],
            }
        );

        let input0 = "\"/([^:/$]+)/Ri, flow:rce_server\"";
        let (_, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(
            pcre,
            Pcre {
                negate: false,
                pattern: r#"([^:/$]+)"#.to_string(),
                modifiers: "Ri".to_string(),
                vars: vec!["flow:rce_server".to_string()],
            }
        );

        let input0 = r#""/\/winhost(?:32|64)\.(exe|pack)$/i""#;
        let (_, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(
            pcre,
            Pcre {
                negate: false,
                pattern: r#"\/winhost(?:32|64)\.(exe|pack)$"#.to_string(),
                modifiers: "i".to_string(),
                vars: vec![],
            }
        );

        let input0 = r#""/\/(?=[0-9]*?[a-z]*?[a-z0-9)(?=[a-z0-9]*[0-9][a-z]*[0-9][a-z0-9]*\.exe)(?!setup\d+\.exe)[a-z0-9]{5,15}\.exe/""#;
        let (_, _pcre) = parse_pcre(input0).unwrap();

        let input0 = r#""/passwd/main\x2Ephp\x3F[^\x0A\x0D]*backend\x3D[^\x0A\x0D\x26]*\x22/i""#;
        let (_, _pcre) = parse_pcre(input0).unwrap();

        let input0 = r#""/^(?:d(?:(?:ocu|uco)sign|ropbox)|o(?:ffice365|nedrive)|adobe|gdoc)/""#;
        let (_, _pcre) = parse_pcre(input0).unwrap();

        let input0 = r#"!"/^onedrivecl[a-z]{2}prod[a-z]{2}[0-9]{5}\./""#;
        let (_, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(rem, "");
        assert!(pcre.negate);

        let input0 = r#"! "/^\w+\s+\w+:\/\/([^\/\s:#]+)[\/\s:#]\S*.+?Host:[ \t]*\1\S*\b/is""#;
        let (rem, pcre) = parse_pcre(input0).unwrap();
        assert_eq!(rem, "");
        assert_eq!(pcre.modifiers, "is");
        assert_eq!(
            pcre.pattern,
            r#"^\w+\s+\w+:\/\/([^\/\s:#]+)[\/\s:#]\S*.+?Host:[ \t]*\1\S*\b"#
        );
    }
}
