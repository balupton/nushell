use nu_engine::{eval_block, CallExt};
use nu_protocol::ast::{Call, CellPath, PathMember};
use nu_protocol::engine::{Closure, Command, EngineState, Stack};
use nu_protocol::{
    Category, Example, FromValue, IntoInterruptiblePipelineData, IntoPipelineData, PipelineData,
    Record, ShellError, Signature, Span, SyntaxShape, Type, Value,
};

#[derive(Clone)]
pub struct Update;

impl Command for Update {
    fn name(&self) -> &str {
        "update"
    }

    fn signature(&self) -> Signature {
        Signature::build("update")
            .input_output_types(vec![
                (Type::Record(vec![]), Type::Record(vec![])),
                (Type::Table(vec![]), Type::Table(vec![])),
                (
                    Type::List(Box::new(Type::Any)),
                    Type::List(Box::new(Type::Any)),
                ),
            ])
            .required(
                "field",
                SyntaxShape::CellPath,
                "the name of the column to update",
            )
            .required(
                "replacement value",
                SyntaxShape::Any,
                "the new value to give the cell(s), or a closure to create the value",
            )
            .allow_variants_without_examples(true)
            .category(Category::Filters)
    }

    fn usage(&self) -> &str {
        "Update an existing column to have a new value."
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        update(engine_state, stack, call, input)
    }

    fn examples(&self) -> Vec<Example> {
        vec![
            Example {
                description: "Update a column value",
                example: "{'name': 'nu', 'stars': 5} | update name 'Nushell'",
                result: Some(Value::test_record(Record {
                    cols: vec!["name".into(), "stars".into()],
                    vals: vec![Value::test_string("Nushell"), Value::test_int(5)],
                })),
            },
            Example {
                description: "Use in closure form for more involved updating logic",
                example: "[[count fruit]; [1 'apple']] | enumerate | update item.count {|e| ($e.item.fruit | str length) + $e.index } | get item",
                result: Some(Value::list(
                    vec![Value::test_record(Record {
                        cols: vec!["count".into(), "fruit".into()],
                        vals: vec![Value::test_int(5), Value::test_string("apple")],
                    })],
                    Span::test_data(),
                )),
            },
            Example {
                description: "Alter each value in the 'authors' column to use a single string instead of a list",
                example: "[[project, authors]; ['nu', ['Andrés', 'JT', 'Yehuda']]] | update authors {|row| $row.authors | str join ','}",
                result: Some(Value::list(
                    vec![Value::test_record(Record {
                        cols: vec!["project".into(), "authors".into()],
                        vals: vec![Value::test_string("nu"), Value::test_string("Andrés,JT,Yehuda")],
                    })],
                    Span::test_data(),
                )),
            },
            Example {
                description: "You can also use a simple command to update 'authors' to a single string",
                example: "[[project, authors]; ['nu', ['Andrés', 'JT', 'Yehuda']]] | update authors {|| str join ','}",
                result: Some(Value::list(
                    vec![Value::test_record(Record {
                        cols: vec!["project".into(), "authors".into()],
                        vals: vec![Value::test_string("nu"), Value::test_string("Andrés,JT,Yehuda")],
                    })],
                    Span::test_data(),
                )),
            }
        ]
    }
}

fn update(
    engine_state: &EngineState,
    stack: &mut Stack,
    call: &Call,
    input: PipelineData,
) -> Result<PipelineData, ShellError> {
    let span = call.head;

    let cell_path: CellPath = call.req(engine_state, stack, 0)?;
    let replacement: Value = call.req(engine_state, stack, 1)?;

    let redirect_stdout = call.redirect_stdout;
    let redirect_stderr = call.redirect_stderr;

    let engine_state = engine_state.clone();
    let ctrlc = engine_state.ctrlc.clone();

    // Replace is a block, so set it up and run it instead of using it as the replacement
    if replacement.as_block().is_ok() {
        let capture_block: Closure = FromValue::from_value(&replacement)?;
        let block = engine_state.get_block(capture_block.block_id).clone();

        let mut stack = stack.captures_to_stack(&capture_block.captures);
        let orig_env_vars = stack.env_vars.clone();
        let orig_env_hidden = stack.env_hidden.clone();

        input.map(
            move |mut input| {
                // with_env() is used here to ensure that each iteration uses
                // a different set of environment variables.
                // Hence, a 'cd' in the first loop won't affect the next loop.
                stack.with_env(&orig_env_vars, &orig_env_hidden);

                if let Some(var) = block.signature.get_positional(0) {
                    if let Some(var_id) = &var.var_id {
                        stack.add_var(*var_id, input.clone())
                    }
                }

                let input_at_path = match input.clone().follow_cell_path(&cell_path.members, false)
                {
                    Err(e) => return Value::error(e, span),
                    Ok(v) => v,
                };
                let output = eval_block(
                    &engine_state,
                    &mut stack,
                    &block,
                    input_at_path.into_pipeline_data(),
                    redirect_stdout,
                    redirect_stderr,
                );

                match output {
                    Ok(pd) => {
                        if let Err(e) =
                            input.update_data_at_cell_path(&cell_path.members, pd.into_value(span))
                        {
                            return Value::error(e, span);
                        }

                        input
                    }
                    Err(e) => Value::error(e, span),
                }
            },
            ctrlc,
        )
    } else {
        if let Some(PathMember::Int { val, span, .. }) = cell_path.members.get(0) {
            let mut input = input.into_iter();
            let mut pre_elems = vec![];

            for idx in 0..*val {
                if let Some(v) = input.next() {
                    pre_elems.push(v);
                } else if idx == 0 {
                    return Err(ShellError::AccessEmptyContent { span: *span });
                } else {
                    return Err(ShellError::AccessBeyondEnd {
                        max_idx: idx - 1,
                        span: *span,
                    });
                }
            }

            // Skip over the replaced value
            let _ = input.next();

            return Ok(pre_elems
                .into_iter()
                .chain(vec![replacement])
                .chain(input)
                .into_pipeline_data(ctrlc));
        }
        input.map(
            move |mut input| {
                let replacement = replacement.clone();

                if let Err(e) = input.update_data_at_cell_path(&cell_path.members, replacement) {
                    return Value::error(e, span);
                }

                input
            },
            ctrlc,
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_examples() {
        use crate::test_examples;

        test_examples(Update {})
    }
}
