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

/// Best of three timings of `run` on the input `prepare` builds for size `n` and for `4 * n`,
/// as the ratio of the larger to the smaller: about 4 for linear cost, 16 for quadratic.
pub fn growth_on_quadrupling<T>(
    n: usize,
    prepare: impl Fn(usize) -> T,
    mut run: impl FnMut(&mut T),
) -> f64 {
    let mut best_of_three = |size: usize| {
        (0..3)
            .map(|_| {
                let mut input = prepare(size);
                let started = std::time::Instant::now();
                run(&mut input);
                started.elapsed()
            })
            .min()
            .expect("three runs")
    };
    let small = best_of_three(n);
    let large = best_of_three(4 * n);
    large.as_secs_f64() / small.as_secs_f64()
}
