use super::artifact;
use wasmparser::{Parser, Payload};

#[test]
fn annotation_imports_become_local_traps_with_valid_function_indices() {
    let input = wat::parse_str(
        r#"(module
      (import "weval" "read.reg" (func (param i64) (result i64)))
      (import "env" "ll_read" (func (param i32)))
      (import "weval" "pop.context" (func))
      (import "env" "ll_write" (func (param i32 i32)))
      (table 1 funcref) (elem (i32.const 0) 4)
      (func (export "run") i32.const 1 call 1 i32.const 2 i32.const 3 call 3 call 2)
    )"#,
    )
    .unwrap();
    let output = artifact::executable(&input).unwrap();
    let mut imports = Vec::new();
    for p in Parser::new(0).parse_all(&output) {
        if let Payload::ImportSection(s) = p.unwrap() {
            for i in s.into_imports() {
                let i = i.unwrap();
                imports.push((i.module.to_string(), i.name.to_string()));
            }
        }
    }
    assert_eq!(
        imports,
        [
            ("env".into(), "ll_read".into()),
            ("env".into(), "ll_write".into())
        ]
    );
    // Idempotence also checks already-generated modules with no annotations.
    assert_eq!(artifact::executable(&output).unwrap(), output);
}

#[test]
fn duplicate_store_import_does_not_satisfy_two_import_contract() {
    let input = wat::parse_str(
        r#"(module
      (import "env" "ll_read" (func)) (import "env" "ll_read" (func))
    )"#,
    )
    .unwrap();
    assert!(artifact::executable(&input).is_err());
}

#[test]
fn checkpoint_replaces_old_data_and_grows_initial_memory() {
    let input = wat::parse_str(
        r#"(module
      (memory (export "memory") 1)
      (global (export "__stack_pointer") (mut i32) (i32.const 100))
      (data (i32.const 0) "old")
    )"#,
    )
    .unwrap();
    let mut memory = vec![0; 2 * 65536];
    memory[70000..70003].copy_from_slice(b"new");
    let output = artifact::checkpoint(&input, &memory, 200).unwrap();
    let mut restored = vec![0; memory.len()];
    for p in Parser::new(0).parse_all(&output) {
        match p.unwrap() {
            Payload::MemorySection(s) => {
                assert_eq!(s.into_iter().next().unwrap().unwrap().initial, 2)
            }
            Payload::GlobalSection(s) => {
                let g = s.into_iter().next().unwrap().unwrap();
                assert!(matches!(
                    g.init_expr.get_operators_reader().read().unwrap(),
                    wasmparser::Operator::I32Const { value: 200 }
                ));
            }
            Payload::DataSection(s) => {
                for d in s {
                    let d = d.unwrap();
                    let wasmparser::DataKind::Active { offset_expr, .. } = d.kind else {
                        panic!()
                    };
                    let wasmparser::Operator::I32Const { value } =
                        offset_expr.get_operators_reader().read().unwrap()
                    else {
                        panic!()
                    };
                    restored[value as usize..value as usize + d.data.len()].copy_from_slice(d.data);
                }
            }
            _ => {}
        }
    }
    assert_eq!(restored, memory);
}

#[test]
fn checkpoint_refuses_unrepresented_global_state() {
    let input = wat::parse_str(
        r#"(module
      (memory 1) (global (export "__stack_pointer") (mut i32) (i32.const 0))
      (global (mut i64) (i64.const 17)) (data (i32.const 0) "x")
    )"#,
    )
    .unwrap();
    assert!(artifact::checkpoint(&input, &vec![0; 65536], 0).is_err());
}
