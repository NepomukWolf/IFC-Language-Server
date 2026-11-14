use crate::document::{EntityInstanceCollection, for_each_entity_instance};
use crate::step::Document;

/// Every entity instance with all of its parameters.
pub fn build_entity_instances(parsed: Option<&Document>, text: &str) -> EntityInstanceCollection {
    let mut instances = Vec::new();
    if let Some(parsed) = parsed {
        for_each_entity_instance(parsed, text, |instance| instances.push(instance));
    }
    EntityInstanceCollection { instances }
}

/// How much longer `run` takes on one input of size `16 * n` than on sixteen inputs of size `n`
/// that `prepare` builds, each the best of five timings: about 1 for linear cost, 16 for quadratic.
pub fn growth_on_sixteenfold<T>(
    n: usize,
    prepare: impl Fn(usize) -> T,
    mut run: impl FnMut(&mut T),
) -> f64 {
    let mut best_of_five = |count: usize, size: usize| {
        (0..5)
            .map(|_| {
                let mut inputs: Vec<T> = (0..count).map(|_| prepare(size)).collect();
                let started = std::time::Instant::now();
                inputs.iter_mut().for_each(&mut run);
                started.elapsed()
            })
            .min()
            .expect("five runs")
    };
    let sixteen_small = best_of_five(16, n);
    let one_large = best_of_five(1, 16 * n);
    one_large.as_secs_f64() / sixteen_small.as_secs_f64()
}

/// `growth_on_sixteenfold` of `run` on texts in which a comment opens again and again inside
/// strings while no `*/` follows, or only one far away; each named by what it holds.
pub fn growth_on_repeated_comment_openings(
    n: usize,
    run: impl Fn(&str),
) -> Vec<(&'static str, f64)> {
    [
        (
            "`; /*` in one unclosed string",
            "DATA;\n#1=IFCWALL('a",
            "; /*",
            "",
        ),
        (
            "`#1/*` lines after an unclosed string",
            "DATA;\n#1=IFCWALL('a\n",
            "#1/*\n",
            "",
        ),
        (
            "closed strings each holding `; /*`",
            "DATA;\n",
            "#1=X('; /*');\n",
            "",
        ),
        (
            "`; /*` in one unclosed string, then `*/` and a definition",
            "DATA;\n#1=IFCWALL('a",
            "; /*",
            "*/#2=",
        ),
    ]
    .into_iter()
    .map(|(what, head, unit, tail)| {
        let growth = growth_on_sixteenfold(
            n,
            |units| format!("{head}{}{tail}", unit.repeat(units)),
            |text| run(text),
        );
        (what, growth)
    })
    .collect()
}
