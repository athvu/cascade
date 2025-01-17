// Copyright (c) Microsoft Corporation.
// SPDX-License-Identifier: MIT
#[macro_use]
extern crate lalrpop_util;

extern crate thiserror;

mod ast;
mod compile;
mod constants;
mod context;
pub mod error;
mod functions;
mod internal_rep;
mod obj_class;
mod sexp_internal;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ast::{Argument, CascadeString, Declaration, Expression, Policy, PolicyFile};
use crate::error::{CascadeErrors, InternalError, InvalidSystemError, ParseErrorMsg};
use crate::internal_rep::{FunctionMap, ModuleMap, SystemMap, ValidatedModule, ValidatedSystem};

use codespan_reporting::files::SimpleFile;
use lalrpop_util::ParseError as LalrpopParseError;

#[cfg(test)]
use error::ErrorItem;

lalrpop_mod!(#[allow(clippy::all)] pub parser);

/// Compile all systems into a single policy
/// The list of input files should contain filenames of files containing policy to be
/// compiled.
/// Returns a Result containing either a string of CIL policy which is the compiled result or a
/// list of errors.
/// In order to convert the compiled CIL policy into a usable policy, you must use secilc.
pub fn compile_combined(input_files: Vec<&str>) -> Result<String, error::CascadeErrors> {
    let errors = CascadeErrors::new();
    let policies = get_policies(input_files)?;
    let mut res = compile_system_policies_internal(policies, vec!["out".to_string()], true)?;
    let ret = match res.remove(&"out".to_string()) {
        Some(s) => s,
        None => return Err(CascadeErrors::from(InternalError::new())),
    };
    errors.into_result(ret)
}

/// Compile a complete system policy
/// The list of input files should contain filenames of files containing policy to be
/// compiled.
/// The list of system names are the names of the systems to build.
/// Returns a Result containing either a string of CIL policy which is the compiled result or a
/// list of errors.
/// In order to convert the compiled CIL policy into a usable policy, you must use secilc.
pub fn compile_system_policies(
    input_files: Vec<&str>,
    system_names: Vec<String>,
) -> Result<HashMap<String, String>, error::CascadeErrors> {
    let policies = get_policies(input_files)?;
    compile_system_policies_internal(policies, system_names, false)
}

/// Compile all of the system policies
/// The list of input files should contain filenames of files containing policy to be
/// compiled.
/// Returns a Result containing either a string of CIL policy which is the compiled result or a
/// list of errors.
/// In order to convert the compiled CIL policy into a usable policy, you must use secilc.
pub fn compile_system_policies_all(
    input_files: Vec<&str>,
) -> Result<HashMap<String, String>, error::CascadeErrors> {
    let mut system_names = Vec::new();
    let policies = get_policies(input_files)?;
    for p in &policies {
        for e in &p.policy.exprs {
            if let Expression::Decl(Declaration::System(s)) = e {
                system_names.push(s.name.to_string());
            }
        }
    }
    compile_system_policies_internal(policies, system_names, false)
}

fn compile_system_policies_internal(
    mut policies: Vec<PolicyFile>,
    system_names: Vec<String>,
    create_default_system: bool,
) -> Result<HashMap<String, String>, error::CascadeErrors> {
    let mut errors = CascadeErrors::new();

    // Generic initialization
    let mut classlist = obj_class::make_classlist();
    let mut type_map = compile::get_built_in_types_map()?;
    let mut module_map = ModuleMap::new();
    let mut system_map = SystemMap::new();

    // Collect all type declarations
    for p in &policies {
        match compile::extend_type_map(p, &mut type_map) {
            Ok(()) => {}
            Err(e) => {
                errors.append(e);
                continue;
            }
        }
    }

    // Stops if something went wrong for this major step.
    errors = errors.into_result_self()?;

    for p in &policies {
        match compile::get_global_bindings(p, &mut type_map, &mut classlist, &p.file) {
            Ok(()) => {}
            Err(e) => {
                errors.append(e);
                continue;
            }
        }
    }

    errors = errors.into_result_self()?;

    // Generate type aliases
    let t_aliases = compile::collect_aliases(type_map.iter());
    type_map.set_aliases(t_aliases);

    for p in &policies {
        match compile::verify_extends(p, &type_map) {
            Ok(()) => (),
            Err(e) => errors.append(e),
        }
    }

    errors = errors.into_result_self()?;

    // Applies annotations
    {
        let mut tmp_func_map = FunctionMap::new();

        // Collect all function declarations
        for p in &policies {
            let mut m = match compile::build_func_map(&p.policy.exprs, &type_map, None, &p.file) {
                Ok(m) => m,
                Err(e) => {
                    errors.append(e);
                    continue;
                }
            };
            tmp_func_map.append(&mut m);
        }

        // TODO: Validate original functions before adding synthetic ones to avoid confusing errors for users.
        match compile::apply_associate_annotations(&type_map, &tmp_func_map) {
            Ok(exprs) => {
                let pf = PolicyFile::new(
                    Policy::new(exprs),
                    SimpleFile::new(String::new(), String::new()),
                );
                match compile::extend_type_map(&pf, &mut type_map) {
                    Ok(()) => policies.push(pf),
                    Err(e) => errors.append(e),
                }
            }
            Err(e) => errors.append(e),
        }
    }
    // Stops if something went wrong for this major step.
    errors = errors.into_result_self()?;

    // Validate modules
    compile::validate_modules(&policies, &type_map, &mut module_map)?;

    // Generate module aliases
    let m_aliases = compile::collect_aliases(module_map.iter());
    module_map.set_aliases(m_aliases);

    // Validate systems
    compile::validate_systems(&policies, &module_map, &mut system_map)?;

    // Create a default module and default system
    // Insert the default module into the default system and insert the system into the system map
    let mut default_module: ValidatedModule;
    let arg;
    if create_default_system {
        default_module = match ValidatedModule::new(
            CascadeString::from("module"),
            BTreeSet::new(),
            BTreeSet::new(),
            None,
            None,
        ) {
            Ok(m) => m,
            Err(_) => {
                return Err(CascadeErrors::from(InternalError::new()));
            }
        };
        arg = Argument::Var(CascadeString::from("allow"));
        for type_info in type_map.values() {
            default_module.types.insert(type_info);
        }
        let mut configs = BTreeMap::new();
        configs.insert(constants::HANDLE_UNKNOWN_PERMS.to_string(), &arg);
        let mut default_system = ValidatedSystem::new(
            CascadeString::from(system_names.first().unwrap().clone()),
            BTreeSet::new(),
            configs,
            None,
        );
        default_system.modules.insert(&default_module);
        system_map.insert(default_system.name.to_string(), default_system)?;
    }

    let mut system_hashmap = HashMap::new();
    for system_name in system_names {
        match system_map.get(&system_name) {
            Some(system) => {
                let system_cil_tree = compile::get_reduced_infos(
                    &policies,
                    &classlist,
                    system,
                    &type_map,
                    &module_map,
                )?;

                let system_cil = generate_cil(system_cil_tree);

                system_hashmap.insert(system_name, system_cil);
            }
            None => errors.append(CascadeErrors::from(InvalidSystemError::new(&format!(
                "System {} does not exist.\nThe valid systems are {}",
                system_name,
                system_map
                    .values()
                    .map(|s| s.name.as_ref())
                    .collect::<Vec<&str>>()
                    .join(", ")
            )))),
        }
    }
    errors.into_result(system_hashmap)
}

fn get_policies(input_files: Vec<&str>) -> Result<Vec<PolicyFile>, CascadeErrors> {
    let mut errors = CascadeErrors::new();
    let mut policies: Vec<PolicyFile> = Vec::new();
    for f in input_files {
        let policy_str = match std::fs::read_to_string(&f) {
            Ok(s) => s,
            Err(e) => {
                errors.add_error(e);
                continue;
            }
        };
        let p = match parse_policy(&policy_str) {
            Ok(p) => p,
            Err(evec) => {
                for e in evec {
                    // TODO: avoid String duplication
                    errors.add_error(error::ParseError::new(e, f.into(), policy_str.clone()));
                }
                continue;
            }
        };
        policies.push(PolicyFile::new(*p, SimpleFile::new(f.into(), policy_str)));
    }
    errors.into_result(policies)
}

fn parse_policy(
    policy: &str,
) -> Result<Box<Policy>, Vec<LalrpopParseError<usize, lalrpop_util::lexer::Token, ParseErrorMsg>>> {
    let mut errors = Vec::new();
    // TODO: Probably should only construct once
    // Why though?
    let parse_res = parser::PolicyParser::new().parse(&mut errors, policy);
    // errors is a vec of ErrorRecovery.  ErrorRecovery is a struct wrapping a ParseError
    // and a sequence of discarded characters.  We don't need those characters, so we just
    // remove the wrapping.
    let mut parse_errors: Vec<LalrpopParseError<usize, lalrpop_util::lexer::Token, ParseErrorMsg>> =
        errors.iter().map(|e| e.error.clone()).collect();
    match parse_res {
        Ok(p) => {
            if !errors.is_empty() {
                // Lalrpop returns errors in the reverse order they were found
                // Reverse so that display is in source line order
                parse_errors.reverse();
                Err(parse_errors)
            } else {
                Ok(p)
            }
        }
        Err(e) => {
            parse_errors.push(e);
            parse_errors.reverse();
            Err(parse_errors)
        }
    }
}

fn generate_cil(v: Vec<sexp::Sexp>) -> String {
    v.iter()
        .map(sexp_internal::display_cil)
        .collect::<Vec<String>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    lalrpop_mod!(#[allow(clippy::all)] pub parser);

    use crate::error::{CompileError, Diag, ParseError};
    use codespan_reporting::diagnostic::Diagnostic;
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::str;

    use super::*;

    const POLICIES_DIR: &str = "data/policies/";
    const ERROR_POLICIES_DIR: &str = "data/error_policies/";
    const EXPECTED_CIL_DIR: &str = "data/expected_cil/";

    #[test]
    fn characterization_tests() {
        let mut count = 0;

        for f in fs::read_dir(POLICIES_DIR).unwrap() {
            count += 1;
            let policy_path = f.unwrap().path();
            let cil_path = match policy_path.extension() {
                Some(e) if e == "cas" => std::path::Path::new(EXPECTED_CIL_DIR).join(
                    policy_path
                        .with_extension("cil")
                        .file_name()
                        .expect(&format!(
                            "failed to extract file name from `{}`",
                            policy_path.to_string_lossy()
                        )),
                ),
                _ => continue,
            };

            // TODO: Make compile_system_policy() take an iterator of AsRef<Path>.
            let cil_gen = match compile_combined(vec![&policy_path.to_string_lossy()]) {
                Ok(c) => c,
                Err(e) => match fs::read_to_string(&cil_path) {
                    Ok(_) => panic!(
                        "Failed to compile '{}', but there is a reference CIL file: {}",
                        policy_path.to_string_lossy(),
                        e
                    ),
                    Err(_) => continue,
                },
            };
            let cil_ref = fs::read_to_string(&cil_path).unwrap_or_else(|e| {
                panic!(
                    "Failed to read file '{}': {}. \
                    You may want to create it with tools/update-expected-cil.sh",
                    cil_path.to_string_lossy(),
                    e
                )
            });
            if cil_gen != cil_ref {
                panic!(
                    "CIL generation doesn't match the recorded one for '{}'. \
                    You may want to update it with tools/update-expected-cil.sh",
                    policy_path.to_string_lossy()
                )
            }
        }

        // Make sure we don't check an empty directory.
        assert!(count > 9);
    }

    fn valid_policy_test(filename: &str, expected_contents: &[&str], disallowed_contents: &[&str]) {
        let policy_file = [POLICIES_DIR, filename].concat();
        let policy_contents = match compile_combined(vec![&policy_file]) {
            Ok(p) => p,
            Err(e) => panic!("Compilation of {} failed with {}", filename, e),
        };
        for query in expected_contents {
            assert!(
                policy_contents.contains(query),
                "Output policy does not contain {}",
                query
            );
        }
        for query in disallowed_contents {
            assert!(
                !policy_contents.contains(query),
                "Output policy contains {}",
                query
            );
        }
        let file_out_path = &[filename, "_test.cil"].concat();
        let cil_out_path = &[filename, "_test_out_policy"].concat();
        let mut out_file = fs::File::create(&file_out_path).unwrap();
        out_file.write_all(policy_contents.as_bytes()).unwrap();
        let output = Command::new("secilc")
            .arg(["--output=", cil_out_path].concat())
            .arg(file_out_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "secilc compilation of {} failed with {}",
            filename,
            str::from_utf8(&output.stderr).unwrap()
        );
        let mut err = false;
        for f in &[file_out_path, cil_out_path] {
            err |= fs::remove_file(f).is_err();
        }
        assert!(!err, "Error removing generated policy files");
    }
    macro_rules! error_policy_test {
        ($filename:literal, $expected_error_count:literal, $error_pattern:pat_param $(if $guard:expr)?) => {
            let policy_file = [ERROR_POLICIES_DIR, $filename].concat();
            match compile_combined(vec![&policy_file]) {
                Ok(_) => panic!("{} compiled successfully", $filename),
                Err(e) => {
                    assert_eq!(e.error_count(), $expected_error_count);
                    for error in e {
                        assert!(matches!(error, $error_pattern $(if $guard)?));
                    }
                }
            }
        }
    }

    #[test]
    fn basic_expression_parse_test() {
        let mut errors = Vec::new();
        let res = parser::ExprParser::new().parse(&mut errors, "domain foo {}");
        assert!(res.is_ok(), "Parse Error: {:?}", res);

        let res = parser::ExprParser::new().parse(&mut errors, "virtual resource foo {}");
        assert!(res.is_ok(), "Parse Error: {:?}", res);

        let res = parser::ExprParser::new().parse(&mut errors, "this.read();");
        assert!(res.is_ok(), "Parse Error: {:?}", res);

        assert_eq!(errors.len(), 0);
    }

    #[test]
    fn name_decl_test() {
        let mut errors = Vec::new();
        for name in &["a", "a_a", "a_a_a", "a_aa_a", "a0", "a_0", "a0_00"] {
            let _: ast::CascadeString = parser::NameDeclParser::new()
                .parse(&mut errors, name)
                .expect(&format!("failed to validate `{}`", name));
        }
        for name in &[
            "0", "0a", "_", "_a", "a_", "a_a_", "a__a", "a__a_a", "a_a___a", "-", "a-a",
        ] {
            let _: LalrpopParseError<_, _, _> = parser::NameDeclParser::new()
                .parse(&mut errors, name)
                .expect_err(&format!("successfully validated invalid `{}`", name));
        }
        assert_eq!(errors.len(), 0)
    }

    #[test]
    fn basic_policy_parse_test() {
        let mut errors = Vec::new();
        let policy_file = [POLICIES_DIR, "tmp_file.cas"].concat();
        let policy = fs::read_to_string(policy_file).unwrap();

        let res = parser::PolicyParser::new().parse(&mut errors, &policy);
        assert!(res.is_ok(), "Parse Error: {:?}", res);
        assert_eq!(errors.len(), 0);
    }

    #[test]
    fn attributes_test() {
        valid_policy_test(
            "attribute.cas",
            &[
                "attribute user_type",
                "type staff",
                "typeattributeset user_type (staff)",
                "typeattributeset domain (user_type)",
            ],
            &[],
        );
    }

    #[test]
    fn simple_policy_build_test() {
        valid_policy_test("simple.cas", &[], &[]);
    }

    #[test]
    fn function_build_test() {
        valid_policy_test(
            "function.cas",
            &["macro my_file-read", "call my_file-read", "allow source"],
            &[],
        );
    }

    #[test]
    fn auditallow_test() {
        valid_policy_test("auditallow.cas", &["auditallow my_domain foo"], &[]);
    }

    #[test]
    fn dontaudit_test() {
        valid_policy_test("dontaudit.cas", &["(dontaudit my_domain foo"], &[]);
    }

    #[test]
    fn arguments_test() {
        valid_policy_test(
            "arguments.cas",
            &["(macro foo-some_func ((type this) (name a) (name b) (type c) (type d))"],
            &[],
        );
    }

    #[test]
    fn filecon_test() {
        valid_policy_test(
            "filecon.cas",
            &["(filecon \"/bin\" file (", "(filecon \"/bin\" dir ("],
            &[],
        );
    }

    #[test]
    fn domtrans_test() {
        valid_policy_test(
            "domtrans.cas",
            &["typetransition bar foo_exec process foo"],
            &[],
        );
    }

    #[test]
    fn symbol_binding_test() {
        valid_policy_test(
            "let.cas",
            &["(allow foo bar (file (read open getattr)))"],
            &[],
        );
    }

    #[test]
    fn virtual_function_test() {
        valid_policy_test(
            "virtual_function.cas",
            &["macro foo-foo"],
            &["macro foo_parent-foo"],
        );
    }

    #[test]
    fn alias_test() {
        valid_policy_test(
            "alias.cas",
            &[
                "(typealias bar)",
                "(typealiasactual bar baz)",
                "macro baz-read",
                "macro bar-list",
                "macro bar-read",
                "macro foo-list",
                "macro foo-read",
                "macro baz-list",
            ],
            &[],
        )
    }

    #[test]
    fn named_args_test() {
        valid_policy_test(
            "named_args.cas",
            &[
                "(call some_domain-three_args (some_domain bar baz foo))",
                "(call some_domain-three_args (some_domain foo bar baz))",
            ],
            &[],
        );
    }

    // TODO:  This test doesn't do much yet.  With just parser support the
    // conditionals just ignore both blocks and generate no policy
    // Once conditionals are actually working, we should add a bunch more
    // cases and add positive checks for the arms that should be included
    // and negative for the ones that shouldn't (and for runtime conditionals
    // we'll need to see both since the condition gets passed through to the
    // final policy in the form of booleans and cil conditionals
    // For now, this confirms that conditionals parse correctly
    #[test]
    fn conditional_test() {
        valid_policy_test(
            "conditional.cas",
            &[], // TODO
            &[],
        );
    }

    #[test]
    fn default_arg_test() {
        valid_policy_test(
            "default.cas",
            &["(call foo-read (foo bar))", "(call foo-read (foo baz))"],
            &[],
        );
    }

    // TODO: Add expected contents list to tests that contain modules
    // after module implementation is complete.
    #[test]
    fn alias_module_test() {
        valid_policy_test("module_alias.cas", &[], &[])
    }

    #[test]
    fn arguments_module_test() {
        valid_policy_test("module_arguments.cas", &[], &[])
    }

    #[test]
    fn simple_module_test() {
        valid_policy_test("module_simple.cas", &[], &[])
    }

    #[test]
    fn system_test() {
        valid_policy_test("systems.cas", &["(handleunknown allow)"], &[]);
    }

    #[test]
    fn extend_test() {
        valid_policy_test(
            "extend.cas",
            &[
                "(allow bar foo (file (getattr)))",
                "(allow bar foo (file (write)))",
                "(macro foo-my_func ((type this) (type source)) (allow source foo (file (read))))",
            ],
            &[],
        );
    }

    #[test]
    fn makelist_test() {
        let policy_file = [POLICIES_DIR, "makelist.cas"].concat();

        match compile_combined(vec![&policy_file]) {
            Ok(_p) => {
                // TODO: reenable.  See note in data/policies/makelist.cas
                //assert!(p.contains(
                //    "(call foo.foo_func"
                //));
            }
            Err(e) => panic!("Makelist compilation failed with {}", e),
        }
    }

    #[test]
    fn multifiles_test() {
        // valid_policy_test() is somewhat tightly wound to the one file case, so we'll code our
        // own copy here
        let policy_files = vec![
            [POLICIES_DIR, "multifile1.cas"].concat(),
            [POLICIES_DIR, "multifile2.cas"].concat(),
        ];
        let policy_files: Vec<&str> = policy_files.iter().map(|s| s as &str).collect();
        let mut policy_files_reversed = policy_files.clone();
        policy_files_reversed.reverse();

        for files in [policy_files, policy_files_reversed] {
            match compile_combined(files) {
                Ok(p) => {
                    assert!(p.contains("(call foo-read"));
                }
                Err(e) => panic!("Multi file compilation failed with {}", e),
            }
        }
    }

    #[test]
    fn compile_system_policies_test() {
        let policy_files = vec![
            [POLICIES_DIR, "system_building1.cas"].concat(),
            [POLICIES_DIR, "system_building2.cas"].concat(),
            [POLICIES_DIR, "system_building3.cas"].concat(),
        ];
        let policy_files: Vec<&str> = policy_files.iter().map(|s| s as &str).collect();
        let system_names = vec!["foo".to_string(), "bar".to_string()];

        let res = compile_system_policies(policy_files, system_names.clone());
        match res {
            Ok(hashmap) => {
                assert_eq!(hashmap.len(), 2);
                for (system_name, system_cil) in hashmap.iter() {
                    if system_name == "foo" {
                        assert!(system_cil.contains("(handleunknown reject)"));
                        assert!(system_cil.contains("(allow thud babble (file (read)))"));
                        assert!(system_cil.contains("(allow thud babble (file (write)))"));
                        assert!(system_cil.contains("(typeattributeset quux (qux))"));
                        assert!(system_cil.contains("(macro qux-read ((type this) (type source)) (allow source qux (file (read))))"));
                        assert!(system_cil.contains("(typeattributeset domain (xyzzy))"));
                        assert!(system_cil.contains("(typeattributeset domain (baz))"));
                        assert!(system_cil.contains("(typeattributeset domain (quuz))"));

                        assert!(!system_cil.contains("(type unused)"));
                    } else {
                        assert!(system_cil.contains("(handleunknown deny)"));
                        assert!(system_cil.contains("(typeattributeset domain (baz))"));
                        assert!(system_cil.contains("(typeattributeset domain (quuz))"));
                        assert!(system_cil.contains("(typeattributeset quux (qux))"));
                        assert!(system_cil.contains("(macro qux-read ((type this) (type source)) (allow source qux (file (read))))"));

                        assert!(!system_cil.contains("(type thud)"));
                        assert!(!system_cil.contains("(type babble)"));
                        assert!(!system_cil.contains("(type xyzzy)"));
                        assert!(!system_cil.contains("(type unused)"));
                    }
                }
            }
            Err(e) => panic!("System building compilation failed with {}", e),
        }
    }

    #[test]
    fn compile_system_policies_all_test() {
        let policy_files = vec![
            [POLICIES_DIR, "system_building1.cas"].concat(),
            [POLICIES_DIR, "system_building2.cas"].concat(),
            [POLICIES_DIR, "system_building3.cas"].concat(),
        ];
        let policy_files: Vec<&str> = policy_files.iter().map(|s| s as &str).collect();

        let res = compile_system_policies_all(policy_files);
        match res {
            Ok(hashmap) => {
                assert_eq!(hashmap.len(), 3);
                for (system_name, system_cil) in hashmap.iter() {
                    if system_name == "foo" {
                        assert!(system_cil.contains("(handleunknown reject)"));
                        assert!(system_cil.contains("(allow thud babble (file (read)))"));
                        assert!(system_cil.contains("(allow thud babble (file (write)))"));
                        assert!(system_cil.contains("(typeattributeset quux (qux))"));
                        assert!(system_cil.contains("(macro qux-read ((type this) (type source)) (allow source qux (file (read))))"));
                        assert!(system_cil.contains("(typeattributeset domain (xyzzy))"));
                        assert!(system_cil.contains("(typeattributeset domain (baz))"));
                        assert!(system_cil.contains("(typeattributeset domain (quuz))"));

                        assert!(!system_cil.contains("(type unused)"));
                    } else if system_name == "bar" {
                        assert!(system_cil.contains("(handleunknown deny)"));
                        assert!(system_cil.contains("(typeattributeset domain (baz))"));
                        assert!(system_cil.contains("(typeattributeset domain (quuz))"));
                        assert!(system_cil.contains("(typeattributeset quux (qux))"));
                        assert!(system_cil.contains("(macro qux-read ((type this) (type source)) (allow source qux (file (read))))"));

                        assert!(!system_cil.contains("(type thud)"));
                        assert!(!system_cil.contains("(type babble)"));
                        assert!(!system_cil.contains("(type xyzzy)"));
                        assert!(!system_cil.contains("(type unused)"));
                    } else {
                        assert!(system_cil.contains("(handleunknown allow)"));
                        assert!(system_cil.contains("(typeattributeset resource (unused))"));

                        assert!(!system_cil.contains("(type thud)"));
                        assert!(!system_cil.contains("(type babble)"));
                        assert!(!system_cil.contains("(type xyzzy)"));
                        assert!(!system_cil.contains("(type baz)"));
                        assert!(!system_cil.contains("(type quuz)"));
                        assert!(!system_cil.contains("(type qux)"));
                    }
                }
            }
            Err(e) => panic!("System building compilation failed with {}", e),
        }
    }

    #[test]
    fn cycle_error_test() {
        error_policy_test!("cycle.cas", 2, ErrorItem::Compile(_));
    }

    #[test]
    fn bad_type_error_test() {
        error_policy_test!("nonexistent_inheritance.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn bad_allow_rules_test() {
        error_policy_test!("bad_allow.cas", 3, ErrorItem::Compile(_));
    }

    #[test]
    fn non_virtual_inherit_test() {
        error_policy_test!("non_virtual_inherit.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn bad_alias_test() {
        error_policy_test!("alias.cas", 2, ErrorItem::Compile(_));
    }

    #[test]
    fn unsupplied_arg_test() {
        error_policy_test!("unsupplied_arg.cas", 1, ErrorItem::Compile(
                CompileError {
                    diagnostic: Diag {
                        inner: Diagnostic {
                            message: msg,
                            ..
                        }
                    },
                    ..
                })
            if msg == *"Function foo.read expected 2 arguments, got 1");
    }

    #[test]
    fn virtual_function_error_test() {
        error_policy_test!("virtual_function_non_define.cas", 1,
            ErrorItem::Compile(CompileError {
                    diagnostic: Diag {
                        inner: Diagnostic {
                            message: msg,
                            ..
                        }
                    },
                    ..
                }) if msg.contains("foo does not define a function named foo_func"));

        error_policy_test!(
            "virtual_function_illegal_call.cas",
            1,
            ErrorItem::Compile(_)
        );
    }

    #[test]
    fn parsing_unrecognized_token() {
        error_policy_test!("parse_unrecognized_token.cas", 1,
            ErrorItem::Parse(ParseError {
                diagnostic: Diag {
                    inner: Diagnostic {
                        message: msg,
                        ..
                    }
                },
                ..
            })
            if msg == *"Unexpected character \".\"");
    }

    #[test]
    fn parsing_unknown_token() {
        error_policy_test!("parse_unknown_token.cas", 1,
            ErrorItem::Parse(ParseError {
                diagnostic: Diag {
                    inner: Diagnostic {
                        message: msg,
                        ..
                    }
                },
                ..
            })
            if msg == *"Unknown character");
    }

    #[test]
    fn parsing_unexpected_eof() {
        error_policy_test!("parse_unexpected_eof.cas", 1,
            ErrorItem::Parse(ParseError {
                diagnostic: Diag {
                    inner: Diagnostic {
                        message: msg,
                        ..
                    }
                },
                ..
            })
            if msg == *"Unexpected end of file");
    }

    #[test]
    fn domain_filecon_test() {
        error_policy_test!("domain_filecon.cas", 1,
        ErrorItem::Compile(CompileError {
                    diagnostic: Diag {
                        inner: Diagnostic {
                            message: msg,
                            ..
                        }
                    },
                    ..
                }) if msg.contains("file_context() calls are only allowed in resources")
        );
    }

    #[test]
    fn virtual_function_associate_error() {
        error_policy_test!("virtual_function_association.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn invalid_module_error() {
        error_policy_test!("module_invalid.cas", 3, ErrorItem::Compile(_));
    }

    #[test]
    fn module_cycle_error() {
        error_policy_test!("module_cycle.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn invalid_system_error() {
        error_policy_test!("system_invalid.cas", 5, ErrorItem::Compile(_));
    }

    #[test]
    fn system_invalid_module_error() {
        error_policy_test!("system_invalid_module.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn system_missing_req_config_error() {
        error_policy_test!("system_missing_req_config.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn system_multiple_config_error() {
        error_policy_test!("system_multiple_config.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn system_no_modules_error() {
        error_policy_test!("system_no_modules.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn system_virtual_error() {
        error_policy_test!("system_virtual.cas", 1, ErrorItem::Parse(ParseError { .. }));
    }

    #[test]
    fn extend_without_declaration_error() {
        error_policy_test!("extend_no_decl.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn extend_double_declaration_error() {
        error_policy_test!("extend_double_decl.cas", 1, ErrorItem::Compile(_));
    }

    #[test]
    fn system_building_error() {
        let policy_files = vec![
            [POLICIES_DIR, "system_building1.cas"].concat(),
            [POLICIES_DIR, "system_building2.cas"].concat(),
            [POLICIES_DIR, "system_building3.cas"].concat(),
        ];
        let policy_files: Vec<&str> = policy_files.iter().map(|s| s as &str).collect();
        let system_names = vec!["baz".to_string()];

        let res = compile_system_policies(policy_files, system_names.clone());
        match res {
            Ok(_) => panic!("Compiled successfully"),
            Err(e) => {
                assert_eq!(e.error_count(), 1);
                for error in e {
                    assert!(matches!(error, ErrorItem::InvalidSystem(_)));
                }
            }
        }
    }

    #[test]
    fn associate_test() {
        valid_policy_test(
            "associate.cas",
            &[
                "call foo-tmp-associated_call_from_tmp (foo-tmp qux)",
                "call bar-tmp-associated_call_from_tmp (bar-tmp qux)",
                "call baz-tmp-associated_call_from_tmp (baz-tmp qux)",
                "call bar-tmp-associated_call_from_tmp (bar-tmp bar)",
                "call bar-var-associated_call_from_var (bar-var bar)",
                "call baz-tmp-associated_call_from_tmp (baz-tmp baz)",
                "call baz-var-associated_call_from_var (baz-var baz)",
                "call foo-tmp-associated_call_from_tmp (foo-tmp foo)",
                "call foo-var-associated_call_from_var (foo-var foo)",
                "call tmp-associated_call_from_tmp (tmp foo)",
                "call tmp-not_an_associated_call (tmp foo)",
                "macro bar-bin-not_an_associated_call_from_bin ((type this) (type source)) (allow source bin (file (read)))",
                "macro bar-tmp-associated_call_from_tmp ((type this) (type source)) (allow source tmp (file (read)))",
                "macro bar-tmp-not_an_associated_call ((type this) (type source)) (allow source tmp (file (write)))",
                "macro bar-var-associated_call_from_var ((type this) (type source)) (allow source var (file (read)))",
                "macro baz-bin-not_an_associated_call_from_bin ((type this) (type source)) (allow source bin (file (read)))",
                "macro baz-tmp-associated_call_from_tmp ((type this) (type source)) (allow source tmp (file (read)))",
                "macro baz-tmp-not_an_associated_call ((type this) (type source)) (allow source tmp (file (write)))",
                "macro baz-var-associated_call_from_var ((type this) (type source)) (allow source var (file (read)))",
                "macro bin-not_an_associated_call_from_bin ((type this) (type source)) (allow source bin (file (read)))",
                "macro foo-tmp-associated_call_from_tmp ((type this) (type source)) (allow source tmp (file (read)))",
                "macro foo-tmp-not_an_associated_call ((type this) (type source)) (allow source tmp (file (write)))",
                "macro foo-var-associated_call_from_var ((type this) (type source)) (allow source var (file (read)))",
                "macro tmp-associated_call_from_tmp ((type this) (type source)) (allow source tmp (file (read)))",
                "macro tmp-not_an_associated_call ((type this) (type source)) (allow source tmp (file (write)))",
                "macro var-associated_call_from_var ((type this) (type source)) (allow source var (file (read)))",
                "type qux",
                "roletype system_r qux",
                "typeattributeset domain (qux)",
                "typeattribute tmp",
                "typeattributeset resource (tmp)",
                "typeattribute bin",
                "typeattributeset resource (bin)",
                "typeattribute foo",
                "typeattributeset domain (foo)",
                "typeattribute var",
                "typeattributeset resource (var)",
                "typeattribute bar",
                "typeattributeset foo (bar)",
                "typeattributeset domain (bar)",
                "typeattribute foo-var",
                "typeattributeset var (foo-var)",
                "typeattributeset resource (foo-var)",
                "typeattribute bar-bin",
                "typeattributeset bin (bar-bin)",
                "typeattributeset resource (bar-bin)",
                "typeattribute foo-tmp",
                "typeattributeset tmp (foo-tmp)",
                "typeattributeset resource (foo-tmp)",
                "typeattribute bar-tmp",
                "typeattributeset foo-tmp (bar-tmp)",
                "typeattributeset resource (bar-tmp)",
                "typeattribute bar-var",
                "typeattributeset foo-var (bar-var)",
                "typeattributeset resource (bar-var)",
                "type baz-var",
                // baz-var must inherit bar-var, not foo-var
                "typeattributeset bar-var (baz-var)",
                "typeattributeset resource (baz-var)",
                "type baz-bin",
                // baz-bin must inherit bar-var, not foo-bin
                "typeattributeset bar-bin (baz-bin)",
                "typeattributeset resource (baz-bin)",
                "type baz-tmp",
                // baz-tmp must inherit bar-tmp, not foo-tmp
                "typeattributeset bar-tmp (baz-tmp)",
                "typeattributeset resource (baz-tmp)",
                "type baz",
                "roletype system_r baz",
                "typeattributeset bar (baz)",
                "typeattributeset domain (baz)",
            ],
            &[]
        );
    }

    #[test]
    fn direct_association_reference_test() {
        valid_policy_test(
            "direct_association_reference.cas",
            &["foo-associated"],
            &["this.associated", "foo.associated", "this-associated"],
        );
    }
}
