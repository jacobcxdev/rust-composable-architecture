use chumsky::prelude::*;
use chumsky::text::{ident, inline_whitespace};

pub type Span = SimpleSpan<usize>;

// https://github.com/inkle/ink-tmlanguage/blob/master/grammars/Ink.YAML-tmLanguage

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum Glue {
    Leading,
    Trailing,
    Both,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Element<'a> {
    Blah,
    Knot(&'a str),
    Stitch(&'a str),
    Choice {
        level: usize,            // The square brackets in fact divide up the option content.
        prompt: Option<&'a str>, // What's before is printed in both choice and output;
        choice: Option<&'a str>, // what's inside only in choice;
        output: Option<&'a str>, // and what's after, only in output.
        glue: Option<Glue>,
        once: bool,
    },
    Gather {
        level: usize,
        prompt: Option<&'a str>,
        glue: Option<Glue>,
    },
    Content {
        text: &'a str,
        glue: Option<Glue>,
        tag: Option<&'a str>,
    },
}

pub fn parser<'a>() -> impl Parser<'a, &'a str, Vec<(Span, Element<'a>)>> {
    // let single_line = just("//").then(any().and_is(just('\n').not()).repeated());
    //
    // let multi_line = just("/*")
    //     .then(any().and_is(just("*/").not()).repeated())
    //     .then_ignore(just("*/"));
    //
    // let comment = single_line.or(multi_line).padded();

    let knot = just('=')
        .repeated()
        .at_least(2)
        .padded_by(inline_whitespace())
        .ignore_then(ident().to_slice())
        .then_ignore(just('=').repeated().padded_by(inline_whitespace().or_not()))
        .map(Element::Knot);

    let stitch = just('=')
        .padded_by(inline_whitespace())
        .ignore_then(ident().padded_by(inline_whitespace()).to_slice())
        .map(Element::Stitch);

    let tag = just('#')
        .ignore_then(just(' ').repeated())
        .ignore_then(any().and_is(just('\n').not()).repeated().to_slice());

    let glue = just("<>").padded_by(inline_whitespace());

    let line = {
        let text = any()
            .and_is(one_of("#\n").not())
            // .and_is(comment.not())
            // .padded_by(comment.repeated())
            .repeated()
            .at_least(1)
            .to_slice();

        glue.or_not()
            .then(text.then(glue.or_not().then(tag.or_not())))
            .map(|(pre, (text, (post, tag)))| {
                let glue = match (pre.is_some(), post.is_some()) {
                    (true, false) => Some(Glue::Leading),
                    (false, true) => Some(Glue::Trailing),
                    (true, true) => Some(Glue::Both),
                    (false, false) => None,
                };

                Element::Content { text, tag, glue }
            })
    };

    let choice = |bullet: char, once: bool| {
        let text = any()
            .and_is(one_of("[]\n").not())
            .repeated()
            .at_least(1)
            // .padded_by(comment.repeated())
            ;

        let bullets = just(bullet)
            .padded_by(inline_whitespace())
            .repeated()
            .at_least(1);

        bullets
            .count()
            .then(text.to_slice().or_not())
            .then(text.to_slice().delimited_by(just('['), just(']')).or_not())
            .then(text.to_slice().or_not())
            .then(glue.to(Glue::Trailing).or_not())
            .map(
                move |((((level, prompt), choice), output), glue)| Element::Choice {
                    level,
                    prompt,
                    choice,
                    output,
                    glue,
                    once,
                },
            )
    };

    let gather = choice('-', false).map(|element| match element {
        Element::Choice {
            level,
            prompt,
            glue,
            ..
        } => Element::Gather {
            level,
            prompt,
            glue,
        },
        _ => unreachable!(),
    });

    knot.or(stitch)
        .or(choice('*', true))
        .or(choice('+', false))
        .or(gather)
        .or(line)
        .map_with(|element, xtra| (xtra.span(), element))
        // .padded_by(comment.repeated())
        .padded()
        .recover_with(skip_then_retry_until(any().ignored(), end()))
        .repeated()
        .collect()
}

#[test]
fn test_parser() {
    let input = r#"
    === knot =======

    = stitch // just for show

    A line of text # with a tag
    * a [single] choice
        ** [another] choice
           with a /* follow up */ line of text
        ** [none]
    * [the best] choice
    +

    <> followed by another line of text.

    "#;

    let parser = parser();
    let result = parser.parse(input);
    // println!("{:?}", result);

    let result = result.into_output_errors();
    println!("{:#?}", result);
}
