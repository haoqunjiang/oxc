use oxc_allocator::String;
use oxc_syntax::identifier::{
    CR, FF, LF, LS, PS, TAB, VT, is_identifier_part, is_identifier_start,
    is_identifier_start_unicode, is_irregular_line_terminator, is_irregular_whitespace,
};

use crate::diagnostics;

use super::{Kind, Lexer, Span};

enum SurrogatePair {
    // valid \u Hex4Digits \u Hex4Digits
    Astral(u32),
    // valid \u Hex4Digits
    CodePoint(u32),
    // invalid \u Hex4Digits \u Hex4Digits
    HighLow(u32, u32),
}

impl<'a> Lexer<'a> {
    pub(super) fn unicode_char_handler(&mut self) -> Kind {
        let c = self.peek_char().unwrap();
        match c {
            c if is_identifier_start_unicode(c) => {
                let start_pos = self.source.position();
                self.consume_char();
                self.identifier_tail_after_unicode(start_pos);
                Kind::Ident
            }
            c if is_irregular_whitespace(c) => {
                self.consume_char();
                self.trivia_builder.add_irregular_whitespace(self.token.start, self.offset());
                Kind::Skip
            }
            c if is_irregular_line_terminator(c) => {
                self.consume_char();
                self.token.is_on_new_line = true;
                self.trivia_builder.add_irregular_whitespace(self.token.start, self.offset());
                Kind::Skip
            }
            _ => {
                self.consume_char();
                self.error(diagnostics::invalid_character(c, self.unterminated_range()));
                Kind::Undetermined
            }
        }
    }

    /// Identifier `UnicodeEscapeSequence`
    ///   \u `Hex4Digits`
    ///   \u{ `CodePoint` }
    pub(super) fn identifier_unicode_escape_sequence(
        &mut self,
        str: &mut String<'a>,
        check_identifier_start: bool,
    ) {
        let start = self.offset();
        if self.peek_byte() == Some(b'u') {
            self.consume_char();
        } else {
            self.next_char();
            let range = Span::new(start, self.offset());
            self.error(diagnostics::unicode_escape_sequence(range));
            return;
        }

        let value = match self.peek_byte() {
            Some(b'{') => self.unicode_code_point(),
            _ => self.surrogate_pair(),
        };

        let Some(value) = value else {
            let range = Span::new(start, self.offset());
            self.error(diagnostics::unicode_escape_sequence(range));
            return;
        };

        // For Identifiers, surrogate pair is an invalid grammar, e.g. `var \uD800\uDEA7`.
        let ch = match value {
            SurrogatePair::Astral(..) | SurrogatePair::HighLow(..) => {
                let range = Span::new(start, self.offset());
                self.error(diagnostics::unicode_escape_sequence(range));
                return;
            }
            SurrogatePair::CodePoint(code_point) => {
                if let Ok(ch) = char::try_from(code_point) {
                    ch
                } else {
                    let range = Span::new(start, self.offset());
                    self.error(diagnostics::unicode_escape_sequence(range));
                    return;
                }
            }
        };

        let is_valid =
            if check_identifier_start { is_identifier_start(ch) } else { is_identifier_part(ch) };

        if !is_valid {
            self.error(diagnostics::invalid_character(ch, self.current_offset()));
            return;
        }

        str.push(ch);
    }

    /// String `UnicodeEscapeSequence`
    ///   \u `Hex4Digits`
    ///   \u `Hex4Digits` \u `Hex4Digits`
    ///   \u{ `CodePoint` }
    fn string_unicode_escape_sequence(
        &mut self,
        text: &mut String<'a>,
        is_valid_escape_sequence: &mut bool,
    ) {
        let value = match self.peek_char() {
            Some('{') => self.unicode_code_point(),
            _ => self.surrogate_pair(),
        };

        let Some(value) = value else {
            // error raised within the parser by `diagnostics::template_literal`
            *is_valid_escape_sequence = false;
            return;
        };

        // For strings and templates, surrogate pairs are valid grammar, e.g. `"\uD83D\uDE00" === 😀`.
        match value {
            SurrogatePair::CodePoint(code_point) | SurrogatePair::Astral(code_point) => {
                if let Ok(ch) = char::try_from(code_point) {
                    text.push(ch);
                } else {
                    // Turns lone surrogate into lossy replacement character (U+FFFD).
                    // A lone surrogate '\u{df06}' is not a valid UTF8 string.
                    text.push_str("\u{FFFD}");
                    self.token.lossy = true;
                }
            }
            SurrogatePair::HighLow(_high, _low) => {
                text.push_str("\u{FFFD}\u{FFFD}");
                self.token.lossy = true;
            }
        }
    }

    fn unicode_code_point(&mut self) -> Option<SurrogatePair> {
        if !self.next_ascii_byte_eq(b'{') {
            return None;
        }
        let value = self.code_point()?;
        if !self.next_ascii_byte_eq(b'}') {
            return None;
        }
        Some(SurrogatePair::CodePoint(value))
    }

    fn hex_4_digits(&mut self) -> Option<u32> {
        let mut value = 0;
        for _ in 0..4 {
            value = (value << 4) | self.hex_digit()?;
        }
        Some(value)
    }

    fn hex_digit(&mut self) -> Option<u32> {
        // Reduce instructions and remove 1 branch by comparing against `A-F` and `a-f` simultaneously
        // https://godbolt.org/z/9caMMzvP3
        let value = if let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                // Match `A-F` or `a-f`. `b | 32` converts uppercase letters to lowercase,
                // but leaves lowercase as they are
                let lower_case = b | 32;
                if matches!(lower_case, b'a'..=b'f') {
                    lower_case + 10 - b'a'
                } else {
                    return None;
                }
            }
        } else {
            return None;
        };
        // Because of `b | 32` above, compiler cannot deduce that next byte is definitely ASCII
        // so `next_byte_unchecked` is necessary to produce compact assembly, rather than `consume_char`.
        // SAFETY: This code is only reachable if there is a byte remaining, and it's ASCII.
        // Therefore it's safe to consume that byte, and will leave position on a UTF-8 char boundary.
        unsafe { self.source.next_byte_unchecked() };
        Some(u32::from(value))
    }

    fn code_point(&mut self) -> Option<u32> {
        let mut value = self.hex_digit()?;
        while let Some(next) = self.hex_digit() {
            value = (value << 4) | next;
            if value > 0x0010_FFFF {
                return None;
            }
        }
        Some(value)
    }

    /// Surrogate pairs
    /// See background info:
    ///   * `https://mathiasbynens.be/notes/javascript-encoding#surrogate-formulae`
    ///   * `https://mathiasbynens.be/notes/javascript-identifiers-es6`
    fn surrogate_pair(&mut self) -> Option<SurrogatePair> {
        let high = self.hex_4_digits()?;
        // The first code unit of a surrogate pair is always in the range from 0xD800 to 0xDBFF, and is called a high surrogate or a lead surrogate.
        let is_pair =
            (0xD800..=0xDBFF).contains(&high) && self.peek_2_bytes() == Some([b'\\', b'u']);
        if !is_pair {
            return Some(SurrogatePair::CodePoint(high));
        }

        self.consume_2_chars();

        let low = self.hex_4_digits()?;

        // The second code unit of a surrogate pair is always in the range from 0xDC00 to 0xDFFF, and is called a low surrogate or a trail surrogate.
        if !(0xDC00..=0xDFFF).contains(&low) {
            return Some(SurrogatePair::HighLow(high, low));
        }

        // `https://tc39.es/ecma262/#sec-utf16decodesurrogatepair`
        let astral_code_point = (high - 0xD800) * 0x400 + low - 0xDC00 + 0x10000;

        Some(SurrogatePair::Astral(astral_code_point))
    }

    // EscapeSequence ::
    pub(super) fn read_string_escape_sequence(
        &mut self,
        text: &mut String<'a>,
        in_template: bool,
        is_valid_escape_sequence: &mut bool,
    ) {
        match self.next_char() {
            None => {
                self.error(diagnostics::unterminated_string(self.unterminated_range()));
            }
            Some(c) => match c {
                // \ LineTerminatorSequence
                // LineTerminatorSequence ::
                // <LF>
                // <CR> [lookahead ≠ <LF>]
                // <LS>
                // <PS>
                // <CR> <LF>
                LF | LS | PS => {}
                CR => {
                    self.next_ascii_byte_eq(b'\n');
                }
                // SingleEscapeCharacter :: one of
                //   ' " \ b f n r t v
                '\'' | '"' | '\\' => text.push(c),
                'b' => text.push('\u{8}'),
                'f' => text.push(FF),
                'n' => text.push(LF),
                'r' => text.push(CR),
                't' => text.push(TAB),
                'v' => text.push(VT),
                // HexEscapeSequence
                'x' => {
                    self.hex_digit()
                        .and_then(|value1| {
                            let value2 = self.hex_digit()?;
                            Some((value1, value2))
                        })
                        .map(|(value1, value2)| (value1 << 4) | value2)
                        .and_then(|value| char::try_from(value).ok())
                        .map_or_else(
                            || {
                                *is_valid_escape_sequence = false;
                            },
                            |c| {
                                text.push(c);
                            },
                        );
                }
                // UnicodeEscapeSequence
                'u' => {
                    self.string_unicode_escape_sequence(text, is_valid_escape_sequence);
                }
                // 0 [lookahead ∉ DecimalDigit]
                '0' if !self.peek_byte().is_some_and(|b| b.is_ascii_digit()) => text.push('\0'),
                // Section 12.9.4 String Literals
                // LegacyOctalEscapeSequence
                // NonOctalDecimalEscapeSequence
                a @ '0'..='7' if !in_template => {
                    let mut num = String::new_in(self.allocator);
                    num.push(a);
                    match a {
                        '4'..='7' => {
                            if matches!(self.peek_byte(), Some(b'0'..=b'7')) {
                                let b = self.consume_char();
                                num.push(b);
                            }
                        }
                        '0'..='3' => {
                            if matches!(self.peek_byte(), Some(b'0'..=b'7')) {
                                let b = self.consume_char();
                                num.push(b);
                                if matches!(self.peek_byte(), Some(b'0'..=b'7')) {
                                    let c = self.consume_char();
                                    num.push(c);
                                }
                            }
                        }
                        _ => {}
                    }

                    let value =
                        char::from_u32(u32::from_str_radix(num.as_str(), 8).unwrap()).unwrap();
                    text.push(value);
                }
                '0' if in_template && self.peek_byte().is_some_and(|b| b.is_ascii_digit()) => {
                    self.consume_char();
                    // error raised within the parser by `diagnostics::template_literal`
                    *is_valid_escape_sequence = false;
                }
                // NotEscapeSequence :: DecimalDigit but not 0
                '1'..='9' if in_template => {
                    // error raised within the parser by `diagnostics::template_literal`
                    *is_valid_escape_sequence = false;
                }
                other => {
                    // NonOctalDecimalEscapeSequence \8 \9 in strict mode
                    text.push(other);
                }
            },
        }
    }
}
