// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module translates the bytecode of a module to Boogie code.

use std::collections::{BTreeMap, BTreeSet};

use itertools::Itertools;
#[allow(unused_imports)]
use log::{debug, info, log, warn, Level};

use spec_lang::{
    code_writer::CodeWriter,
    emit, emitln,
    env::{GlobalEnv, Loc, ModuleEnv, StructEnv, TypeParameter},
    ty::{PrimitiveType, Type},
};
use stackless_bytecode_generator::{
    function_target::FunctionTarget,
    function_target_pipeline::FunctionTargetsHolder,
    graph::{Graph, Reducible},
    stackless_bytecode::{
        AssignKind, BorrowNode,
        Bytecode::{self, *},
        Constant, Label, Operation, SpecBlockId,
    },
    stackless_control_flow_graph::{BlockId, StacklessControlFlowGraph},
};
use vm::file_format::CodeOffset;

use crate::{
    boogie_helpers::{
        boogie_byte_blob, boogie_field_name, boogie_function_name, boogie_local_type,
        boogie_requires_well_formed, boogie_resource_memory_name,
        boogie_saved_resource_memory_name, boogie_struct_name, boogie_type_value,
        boogie_type_value_array, boogie_type_values, boogie_well_formed_check, WellFormedMode,
    },
    cli::Options,
    spec_translator::{FunctionEntryPoint, SpecEnv, SpecTranslator},
};
use spec_lang::env::{
    ADDITION_OVERFLOW_UNCHECKED_PRAGMA, ASSUME_NO_ABORT_FROM_HERE_PRAGMA, OPAQUE_PRAGMA,
    VERIFY_DURATION_ESTIMATE,
};
use std::cell::RefCell;

pub struct BoogieTranslator<'env> {
    env: &'env GlobalEnv,
    writer: &'env CodeWriter,
    options: &'env Options,
    targets: &'env FunctionTargetsHolder,
}

pub struct ModuleTranslator<'env> {
    writer: &'env CodeWriter,
    options: &'env Options,
    module_env: ModuleEnv<'env>,
    targets: &'env FunctionTargetsHolder,
    in_toplevel_verify: RefCell<bool>,
}

/// A struct encapsulating information which is threaded through translating the bytecodes of
/// a single function. This holds information which is relevant across multiple bytecode
/// instructions, like borrowing information and label offsets.
struct BytecodeContext {
    /// Set of mutable references, represented by local index. Used for debug tracking. Currently,
    /// after each mutation (either by an instruction or by call to a function with mutable
    /// parameters), we dump tracking info for all the variables in this set. This is a vast
    /// over-approximation; however, the execution trace visualizer will remove redundant
    /// entries, so it is more of a performance concern.
    mutable_refs: BTreeSet<usize>,
    loop_targets: BTreeMap<Label, BTreeSet<usize>>,
}

impl BytecodeContext {
    fn new(func_target: &FunctionTarget<'_>) -> Self {
        let mutable_refs = Self::collect_mutable_refs(func_target);
        let loop_targets = Self::collect_loop_targets(func_target);
        BytecodeContext {
            mutable_refs,
            loop_targets,
        }
    }

    fn collect_mutable_refs(func_target: &FunctionTarget<'_>) -> BTreeSet<usize> {
        let code = func_target.get_bytecode();
        let mut mutable_refs = BTreeSet::new();
        // Walk over the bytecode and collect various context information.
        for bytecode in code {
            match bytecode {
                Call(_, dsts, oper, _) => {
                    use Operation::*;
                    match oper {
                        BorrowLoc | BorrowGlobal(..) | BorrowField(..) => {
                            let dst = dsts[0];
                            let ty = func_target.get_local_type(dst);
                            if ty.is_mutable_reference() {
                                // Track that we create a mutable reference here.
                                mutable_refs.insert(dst);
                            }
                        }
                        _ => {}
                    }
                }
                Assign(_, dst, src, AssignKind::Move) | Assign(_, dst, src, AssignKind::Store) => {
                    // Propagate information from src to dst.
                    if mutable_refs.contains(src) {
                        mutable_refs.insert(*dst);
                    }
                }
                _ => {}
            }
        }
        mutable_refs
    }

    fn collect_loop_targets(func_target: &FunctionTarget<'_>) -> BTreeMap<Label, BTreeSet<usize>> {
        let code = func_target.get_bytecode();
        let cfg = StacklessControlFlowGraph::new_forward(code);
        let entry = cfg.entry_blocks()[0];
        let nodes = cfg.blocks();
        let edges: Vec<(BlockId, BlockId)> = nodes
            .iter()
            .map(|x| {
                cfg.successors(*x)
                    .iter()
                    .map(|y| (*x, *y))
                    .collect::<Vec<(BlockId, BlockId)>>()
            })
            .flatten()
            .collect();
        let graph = Graph::new(entry, nodes, edges);
        let mut loop_targets = BTreeMap::new();
        if let Some(Reducible {
            loop_headers,
            natural_loops,
        }) = graph.compute_reducible()
        {
            let block_id_to_label: BTreeMap<BlockId, Label> = loop_headers
                .iter()
                .map(|x| {
                    if let Label(_, label) = code[cfg.block_start(*x) as usize] {
                        Some((*x, label))
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();
            for (back_edge, natural_loop) in natural_loops {
                let loop_header_label = block_id_to_label[&back_edge.1];
                loop_targets
                    .entry(loop_header_label)
                    .or_insert_with(BTreeSet::new);
                let natural_loop_targets = natural_loop
                    .iter()
                    .map(|block_id| {
                        cfg.instr_indexes(*block_id)
                            .map(|x| Self::targets(&code[x as usize]))
                            .flatten()
                            .collect::<BTreeSet<usize>>()
                    })
                    .flatten()
                    .collect::<BTreeSet<usize>>();
                for target in natural_loop_targets {
                    loop_targets.entry(loop_header_label).and_modify(|x| {
                        x.insert(target);
                    });
                }
            }
        }
        loop_targets
    }

    fn targets(bytecode: &Bytecode) -> Vec<usize> {
        use BorrowNode::*;
        match bytecode {
            Assign(_, dest, _, _) => vec![*dest],
            Call(_, dests, _, _) => dests.clone(),
            Load(_, dest, _) => vec![*dest],
            WriteBack(_, LocalRoot(dest), _) => vec![*dest],
            WriteBack(_, Reference(dest), _) => vec![*dest],
            Splice(_, dest, _) => vec![*dest],
            _ => vec![],
        }
    }
}

impl<'env> BoogieTranslator<'env> {
    pub fn new(
        env: &'env GlobalEnv,
        options: &'env Options,
        targets: &'env FunctionTargetsHolder,
        writer: &'env CodeWriter,
    ) -> Self {
        Self {
            env,
            targets,
            writer,
            options,
        }
    }

    pub fn translate(&mut self) {
        // generate definitions for all modules.
        for module_env in self.env.get_modules() {
            ModuleTranslator::new(self, module_env).translate();
        }
    }
}

impl<'env> ModuleTranslator<'env> {
    /// Creates a new module translator. Calls the stackless bytecode generator and wraps
    /// result into the translator.
    fn new(parent: &'env BoogieTranslator, module: ModuleEnv<'env>) -> Self {
        Self {
            writer: parent.writer,
            options: parent.options,
            module_env: module,
            targets: &parent.targets,
            in_toplevel_verify: Default::default(),
        }
    }

    fn new_spec_translator_for_module(&self) -> SpecTranslator<'_> {
        self.new_spec_translator(self.module_env.clone(), false)
    }

    fn new_spec_translator<E>(&self, env: E, supports_native_old: bool) -> SpecTranslator<'_>
    where
        E: Into<SpecEnv<'env>>,
    {
        SpecTranslator::new(
            self.writer,
            env,
            self.targets,
            self.options,
            supports_native_old,
        )
    }

    fn set_top_level_verify(&self, value: bool) {
        *self.in_toplevel_verify.borrow_mut() = value;
    }

    fn in_top_level_verify(&self) -> bool {
        *self.in_toplevel_verify.borrow()
    }

    /// Translates this module.
    fn translate(&mut self) {
        log!(
            if self.module_env.is_dependency() {
                Level::Debug
            } else {
                Level::Info
            },
            "translating module {}",
            self.module_env
                .get_name()
                .display(self.module_env.symbol_pool())
        );
        self.writer
            .set_location(&self.module_env.env.internal_loc());
        let spec_translator = self.new_spec_translator_for_module();
        spec_translator.translate_spec_vars();
        spec_translator.translate_spec_funs();
        self.translate_structs();
        self.translate_functions();
    }

    /// Translates all structs in the module.
    fn translate_structs(&self) {
        emitln!(
            self.writer,
            "\n\n// ** structs of module {}\n",
            self.module_env
                .get_name()
                .display(self.module_env.symbol_pool())
        );
        for struct_env in self.module_env.get_structs() {
            // Set the location to internal so we don't see locations of pack/unpack
            // in execution traces.
            self.writer
                .set_location(&self.module_env.env.internal_loc());
            self.translate_struct_type(&struct_env);
            if !struct_env.is_native() {
                self.translate_struct_accessors(&struct_env);
            }
        }
    }

    /// Translates the given struct.
    fn translate_struct_type(&self, struct_env: &StructEnv<'_>) {
        // Emit TypeName
        let struct_name = boogie_struct_name(&struct_env);
        // Special treatment of well-known resource Account_T. The type_name
        // is forward-declared in the prelude.
        if struct_name != "$Account_T" {
            emitln!(self.writer, "const unique {}: $TypeName;", struct_name);
        }

        // Emit FieldNames
        for (i, field_env) in struct_env.get_fields().enumerate() {
            let field_name = boogie_field_name(&field_env);
            emitln!(
                self.writer,
                "const {}: $FieldName;\naxiom {} == {};",
                field_name,
                field_name,
                i
            );
        }

        // Emit TypeValue constructor function.
        let type_params = struct_env
            .get_type_parameters()
            .iter()
            .enumerate()
            .map(|(i, _)| format!("$tv{}: $TypeValue", i))
            .join(", ");
        let type_args = struct_env
            .get_type_parameters()
            .iter()
            .enumerate()
            .map(|(i, _)| Type::TypeParameter(i as u16))
            .collect_vec();
        let type_args_array = boogie_type_value_array(struct_env.module_env.env, &type_args);
        let type_value = format!("$StructType({}, {})", struct_name, type_args_array);
        emitln!(
            self.writer,
            "function {}_type_value({}): $TypeValue {{\n    {}\n}}",
            struct_name,
            type_params,
            type_value
        );

        // Emit memory variables.
        let memory_name =
            boogie_resource_memory_name(struct_env.module_env.env, struct_env.get_qualified_id());
        emitln!(self.writer, "var {}: $Memory;", memory_name);
        let saved_memory_name = boogie_saved_resource_memory_name(
            struct_env.module_env.env,
            struct_env.get_qualified_id(),
        );
        emitln!(self.writer, "var {}: $Memory;", saved_memory_name);

        // Emit invariant functions.
        let spec_translator = self.new_spec_translator(struct_env.clone(), false);
        spec_translator.translate_invariant_functions();
    }

    /// Translates struct accessors (pack/unpack).
    fn translate_struct_accessors(&self, struct_env: &StructEnv<'_>) {
        // Pack function
        let type_args_str = struct_env
            .get_type_parameters()
            .iter()
            .map(|TypeParameter(s, _)| {
                format!("{}: $TypeValue", s.display(struct_env.symbol_pool()))
            })
            .join(", ");
        let args_str = struct_env
            .get_fields()
            .map(|field_env| {
                format!(
                    "{}: $Value",
                    field_env.get_name().display(struct_env.symbol_pool())
                )
            })
            .join(", ");
        emitln!(
            self.writer,
            "procedure {{:inline 1}} {}_pack($file_id: int, $byte_index: int, $var_idx: int, {}) returns ($struct: $Value)\n{{",
            boogie_struct_name(struct_env),
            separate(vec![type_args_str.clone(), args_str.clone()], ", ")
        );
        self.writer.indent();
        // $Vector is either represented using sequences or integer maps
        if self.options.backend.vector_using_sequences {
            // Using sequences as the internal representation
            let mut ctor_expr = "$EmptyValueArray()".to_owned();
            for field_env in struct_env.get_fields() {
                let field_param =
                    &format!("{}", field_env.get_name().display(struct_env.symbol_pool()));
                let type_check = boogie_well_formed_check(
                    self.module_env.env,
                    field_param,
                    &field_env.get_type(),
                    WellFormedMode::Default,
                );
                emit!(self.writer, &type_check);
                // TODO: Remove the use of $ExtendValueArray; it is deprecated
                ctor_expr = format!("$ExtendValueArray({},{})", ctor_expr, field_param);
            }
            emitln!(self.writer, "$struct := $Vector({});", ctor_expr);
        } else {
            // Using integer maps as the internal representation
            let mut ctor_expr = "$MapConstValue($DefaultValue())".to_owned();
            for field_env in struct_env.get_fields() {
                let field_param =
                    &format!("{}", field_env.get_name().display(struct_env.symbol_pool()));
                let type_check = boogie_well_formed_check(
                    self.module_env.env,
                    field_param,
                    &field_env.get_type(),
                    WellFormedMode::Default,
                );
                emit!(self.writer, &type_check);
                ctor_expr = format!(
                    "{}[{} := {}]",
                    ctor_expr,
                    field_env.get_offset(),
                    field_param
                );
            }
            emitln!(
                self.writer,
                "$struct := $Vector($ValueArray({}, {}));",
                ctor_expr,
                struct_env.get_field_count()
            );
        }

        // Generate $DebugTrackLocal so we can see the constructed value before invariant
        // evaluation may abort.
        emitln!(
            self.writer,
            "if ($byte_index > 0) { assume $DebugTrackLocal($file_id, $byte_index, $var_idx, $struct); }"
        );

        // Insert invariant code.
        let spec_translator = self.new_spec_translator(struct_env.clone(), false);
        spec_translator.emit_pack_invariants("$struct");

        self.writer.unindent();
        emitln!(self.writer, "}\n");

        // Unpack function
        emitln!(
            self.writer,
            "procedure {{:inline 1}} {}_unpack({}) returns ({})\n{{",
            boogie_struct_name(struct_env),
            separate(vec![type_args_str, "$struct: $Value".to_string()], ", "),
            args_str
        );
        self.writer.indent();
        emitln!(self.writer, "assume is#$Vector($struct);");
        for field_env in struct_env.get_fields() {
            emitln!(
                self.writer,
                "{} := $SelectField($struct, {});",
                field_env.get_name().display(struct_env.symbol_pool()),
                boogie_field_name(&field_env)
            );
            let type_check = boogie_well_formed_check(
                self.module_env.env,
                &format!("{}", field_env.get_name().display(struct_env.symbol_pool())),
                &field_env.get_type(),
                WellFormedMode::Default,
            );
            emit!(self.writer, &type_check);
        }

        // Insert invariant checking code.
        let spec_translator = self.new_spec_translator(struct_env.clone(), false);
        spec_translator.emit_unpack_invariants("$struct");

        self.writer.unindent();
        emitln!(self.writer, "}\n");
    }

    /// Translates all functions in the module.
    fn translate_functions(&self) {
        emitln!(
            self.writer,
            "\n\n// ** functions of module {}\n",
            self.module_env
                .get_name()
                .display(self.module_env.symbol_pool())
        );
        let mut num_fun_specified = 0;
        let mut num_fun = 0;
        for func_env in self.module_env.get_functions() {
            if !func_env.is_native() {
                num_fun += 1;
            }
            if func_env.get_spec().has_conditions() && !func_env.is_native() {
                num_fun_specified += 1;
            }
            self.writer.set_location(&func_env.get_loc());
            self.translate_function(&self.targets.get_target(&func_env));
        }
        if num_fun > 0 && !self.module_env.is_dependency() {
            debug!(
                "{} out of {} functions have (directly or indirectly) \
                 specifications in module `{}`",
                num_fun_specified,
                num_fun,
                self.module_env
                    .get_name()
                    .display_full(self.module_env.symbol_pool())
            );
        }
    }
}

impl<'env> ModuleTranslator<'env> {
    /// Translates the given function.
    fn translate_function(&self, func_target: &FunctionTarget<'_>) {
        use FunctionEntryPoint::*;
        if func_target.is_native() {
            if self.options.prover.native_stubs {
                self.generate_function_sig(func_target, Indirect);
                emit!(self.writer, ";");
                self.generate_function_spec(func_target, Indirect);
                emitln!(self.writer);
            }
            return;
        }

        // generate definition entry
        self.generate_function_sig(func_target, Definition);
        self.generate_inline_function_body(func_target);
        emitln!(self.writer);

        // generate direct and indirect application entries.
        let opaque = func_target.is_pragma_true(OPAQUE_PRAGMA, || false);
        let entries = if func_target.is_public() {
            vec![DirectInterModule, DirectIntraModule, Indirect]
        } else {
            vec![DirectIntraModule, Indirect]
        };
        for entry_point in entries {
            self.generate_function_sig(func_target, entry_point);
            if opaque {
                emit!(self.writer, ";");
            }
            self.generate_function_spec(func_target, entry_point);
            if !opaque {
                self.generate_function_stub(func_target, entry_point, Definition);
            }
            emitln!(self.writer);
        }

        // If the function is not verified, or the timeout is less than the estimated time,
        // stop here.
        if !func_target
            .func_env
            .should_verify(self.options.prover.verify_scope)
        {
            return;
        }
        if let Some(n) = func_target
            .module_env()
            .env
            .get_num_property(&func_target.get_spec().properties, VERIFY_DURATION_ESTIMATE)
        {
            if n > self.options.backend.vc_timeout {
                info!(
                    "`{}::{}` excluded from verification because it is estimated to take \
                        longer ({}s) to verify than configured timeout ({}s)",
                    func_target
                        .module_env()
                        .get_name()
                        .display(func_target.symbol_pool()),
                    func_target.get_name().display(func_target.symbol_pool()),
                    n,
                    self.options.backend.vc_timeout
                );
                return;
            }
        }

        // generate the verify functions with full pre/post conditions.
        self.generate_function_sig(func_target, FunctionEntryPoint::VerificationDefinition);
        self.set_top_level_verify(true); // Ensure that DirectCall is used from this definition
        self.generate_inline_function_body(func_target);
        self.set_top_level_verify(false);
        emitln!(self.writer);
        self.generate_function_sig(func_target, FunctionEntryPoint::Verification);
        self.generate_function_spec(func_target, FunctionEntryPoint::Verification);
        self.generate_function_stub(
            func_target,
            FunctionEntryPoint::Verification,
            FunctionEntryPoint::VerificationDefinition,
        );
        emitln!(self.writer);
    }

    /// Return a string for a boogie procedure header. Use inline attribute and name
    /// suffix as indicated by `entry_point`.
    fn generate_function_sig(
        &self,
        func_target: &FunctionTarget<'_>,
        entry_point: FunctionEntryPoint,
    ) {
        let (args, rets) = self.generate_function_args_and_returns(func_target);
        let inline = match entry_point {
            FunctionEntryPoint::Definition
            | FunctionEntryPoint::VerificationDefinition
            | FunctionEntryPoint::DirectIntraModule
            | FunctionEntryPoint::Indirect
            | FunctionEntryPoint::DirectInterModule => "{:inline 1} ",
            _ => "",
        };
        let suffix = entry_point.suffix();
        self.writer.set_location(&func_target.get_loc());
        emitln!(
            self.writer,
            "procedure {}{}{}({}) returns ({})",
            inline,
            boogie_function_name(func_target.func_env),
            suffix,
            args,
            rets,
        )
    }

    /// Generate boogie representation of function args and return args.
    fn generate_function_args_and_returns(
        &self,
        func_target: &FunctionTarget<'_>,
    ) -> (String, String) {
        let args = func_target
            .get_type_parameters()
            .iter()
            .map(|TypeParameter(s, _)| {
                format!("{}: $TypeValue", s.display(func_target.symbol_pool()))
            })
            .chain((0..func_target.get_parameter_count()).map(|i| {
                let s = func_target.get_local_name(i);
                let ty = func_target.get_local_type(i);
                format!(
                    "{}: {}",
                    s.display(func_target.symbol_pool()),
                    boogie_local_type(ty)
                )
            }))
            .join(", ");
        let rets = func_target
            .get_return_types()
            .iter()
            .enumerate()
            .map(|(i, ref s)| format!("$ret{}: {}", i, boogie_local_type(s)))
            .join(", ");
        (args, rets)
    }

    /// Generate preconditions to make sure procedure parameters are well formed
    fn generate_function_args_well_formed(&self, func_target: &FunctionTarget<'_>) {
        let num_args = func_target.get_parameter_count();
        let mode = if func_target.is_public() {
            // For public functions, we always include invariants in type assumptions for parameters,
            // even for mutable references.
            WellFormedMode::WithInvariant
        } else {
            WellFormedMode::Default
        };
        for i in 0..num_args {
            let local_name = func_target.get_local_name(i);
            let local_str = format!("{}", local_name.display(func_target.symbol_pool()));
            let local_type = func_target.get_local_type(i);
            let type_check = boogie_requires_well_formed(
                self.module_env.env,
                &local_str,
                local_type,
                mode,
                &self.options.backend.type_requires,
            );
            if !type_check.is_empty() {
                emitln!(self.writer, &type_check);
            }
        }
    }

    /// Emit code for the function specification.
    fn generate_function_spec(
        &self,
        func_target: &FunctionTarget<'_>,
        entry_point: FunctionEntryPoint,
    ) {
        self.new_spec_translator(func_target.clone(), true)
            .translate_conditions(entry_point);
    }

    /// Emit code for spec inside function implementation.
    fn generate_function_spec_inside_impl(
        &self,
        func_target: &FunctionTarget<'_>,
        block_id: SpecBlockId,
    ) {
        self.new_spec_translator(func_target.clone(), true)
            .translate_conditions_inside_impl(block_id);
    }

    /// Generate function stub depending on entry point type. This forwards to the
    /// inlined function definition.
    fn generate_function_stub(
        &self,
        func_target: &FunctionTarget<'_>,
        entry_point: FunctionEntryPoint,
        def_entry_point: FunctionEntryPoint,
    ) {
        // Set the location to internal so it won't be counted for execution traces
        self.writer
            .set_location(&self.module_env.env.internal_loc());
        emitln!(self.writer, "{");
        self.writer.indent();

        // Translate argument type requirements.
        self.generate_function_args_well_formed(func_target);

        // Translate assumptions specific to this entry point.
        let spec_translator = self.new_spec_translator(func_target.clone(), false);
        spec_translator.translate_entry_point_assumptions(entry_point);

        // Generate call to inlined function.
        let args = func_target
            .get_type_parameters()
            .iter()
            .map(|TypeParameter(s, _)| format!("{}", s.display(func_target.symbol_pool())))
            .chain((0..func_target.get_parameter_count()).map(|i| {
                format!(
                    "{}",
                    func_target
                        .get_local_name(i)
                        .display(func_target.symbol_pool())
                )
            }))
            .join(", ");
        let rets = (0..func_target.get_return_count())
            .map(|i| format!("$ret{}", i))
            .join(", ");
        if rets.is_empty() {
            emitln!(
                self.writer,
                "call {}{}({});",
                boogie_function_name(func_target.func_env),
                def_entry_point.suffix(),
                args
            )
        } else {
            emitln!(
                self.writer,
                "call {} := {}{}({});",
                rets,
                boogie_function_name(func_target.func_env),
                def_entry_point.suffix(),
                args
            )
        }
        self.writer.unindent();
        emitln!(self.writer, "}");
        emitln!(self.writer);
    }

    /// This generates boogie code for everything after the function signature
    /// The function body is only generated for the `FunctionEntryPoint::Definition`
    /// version of the function.
    fn generate_inline_function_body(&self, func_target: &FunctionTarget<'_>) {
        // Construct context for bytecode translation.
        let context = BytecodeContext::new(func_target);

        // Be sure to set back location to the whole function definition as a default, otherwise
        // we may get unassigned code locations associated with condition locations.
        self.writer.set_location(&func_target.get_loc());

        emitln!(self.writer, "{");
        self.writer.indent();

        // Generate local variable declarations. They need to appear first in boogie.
        emitln!(self.writer, "// declare local variables");
        let num_args = func_target.get_parameter_count();
        for i in num_args..func_target.get_local_count() {
            let local_name = func_target.get_local_name(i);
            let local_type = func_target.get_local_type(i);
            emitln!(
                self.writer,
                "var {}: {}; // {}",
                local_name.display(func_target.symbol_pool()),
                boogie_local_type(local_type),
                boogie_type_value(self.module_env.env, local_type)
            );
        }
        emitln!(self.writer, "var $tmp: $Value;");

        emitln!(self.writer, "\n// initialize function execution");
        emitln!(self.writer, "assume !$abort_flag;");

        emitln!(self.writer, "\n// track values of parameters at entry time");
        for i in 0..func_target.get_parameter_count() {
            let local_name = func_target.get_local_name(i);
            let local_str = format!("{}", local_name.display(func_target.symbol_pool()));
            let s = self.track_local(func_target, func_target.get_loc(), i, &local_str);
            if !s.is_empty() {
                emitln!(self.writer, &s);
            }
        }

        emitln!(self.writer, "\n// bytecode translation starts here");

        // Generate bytecode
        let code = func_target.get_bytecode();
        for (offset, bytecode) in code.iter().enumerate() {
            self.translate_bytecode(func_target, &context, offset as CodeOffset, bytecode);
        }

        // Generate abort exit.
        let end_loc = func_target.get_loc().at_end();
        self.writer.set_location(&end_loc);
        self.writer.unindent();
        emitln!(self.writer, "Abort:");
        self.writer.indent();
        emitln!(self.writer, "$abort_flag := true;");
        for (i, ty) in func_target.get_return_types().iter().enumerate() {
            let ret_str = format!("$ret{}", i);
            if ty.is_reference() {
                emitln!(self.writer, "{} := $DefaultMutation;", &ret_str);
            } else {
                emitln!(self.writer, "{} := $DefaultValue();", &ret_str);
            }
        }
        self.writer.unindent();
        emitln!(self.writer, "}");
    }

    /// Translates one bytecode instruction.
    fn translate_bytecode(
        &'env self,
        func_target: &FunctionTarget<'_>,
        ctx: &BytecodeContext,
        offset: u16,
        bytecode: &Bytecode,
    ) {
        // Set location of this code in the CodeWriter.
        let loc = func_target.get_bytecode_loc(bytecode.get_attr_id());
        self.writer.set_location(&loc);
        emitln!(self.writer, "// {}", bytecode.display(func_target));

        // Helper function to get an Rc<String> for a local.
        let str_local = |idx: usize| {
            func_target
                .symbol_pool()
                .string(func_target.get_local_name(idx))
        };

        // Helper functions to update a local including debug tracking.
        let update_and_track_local = |idx: usize, value: &str| {
            self.update_and_track_local(func_target, loc.clone(), idx, value)
        };

        // Helper functions to debug track a local.
        let track_local =
            |idx: usize, value: &str| self.track_local(func_target, loc.clone(), idx, value);

        // Helper functions to debug track a return value.
        let track_return = |idx: usize| {
            self.track_local(
                func_target,
                loc.clone(),
                func_target.get_local_count() + idx,
                &format!("$ret{}", idx),
            )
        };

        // Helper function to debug track potential updates of references.
        let track_mutable_refs = |ctx: &BytecodeContext| {
            for idx in &ctx.mutable_refs {
                if *idx < func_target.get_local_count() {
                    let s =
                        self.track_local(func_target, loc.clone(), *idx, str_local(*idx).as_str());
                    if !s.is_empty() {
                        emitln!(self.writer, &s);
                    }
                }
            }
            // Add reference parameter because we also want to debug track them when
            // references are written.
            for idx in 0..func_target.get_parameter_count() {
                let ty = func_target.get_local_type(idx);
                if ty.is_mutable_reference() {
                    let s =
                        self.track_local(func_target, loc.clone(), idx, str_local(idx).as_str());
                    if !s.is_empty() {
                        emitln!(self.writer, &s);
                    }
                }
            }
        };

        let propagate_abort = || {
            format!(
                "if ($abort_flag) {{\n  assume $DebugTrackAbort({}, {});\n  goto Abort;\n}}",
                func_target
                    .func_env
                    .module_env
                    .env
                    .file_id_to_idx(loc.file_id()),
                loc.span().start(),
            )
        };

        // Translate the bytecode instruction.
        match bytecode {
            UnpackRef(_, src) => {
                self.enforce_before_update_invariant(func_target, *src);
            }
            PackRef(_, src) => {
                self.enforce_after_update_invariant(func_target, *src);
            }
            WriteBack(_, dest, src) => {
                use BorrowNode::*;
                match dest {
                    GlobalRoot(struct_decl) => {
                        let memory = struct_decl.module_id.qualified(struct_decl.struct_id);
                        let spec_translator = self.new_spec_translator_for_module();
                        spec_translator.save_memory_for_update_invariants(memory);
                        let memory_name =
                            boogie_resource_memory_name(func_target.global_env(), memory);
                        emitln!(
                            self.writer,
                            "call {} := $WritebackToGlobal({}, {});",
                            memory_name,
                            memory_name,
                            str_local(*src),
                        );
                        spec_translator.emit_global_invariants_for_memory(
                            false, // assert
                            memory,
                        );
                    }
                    LocalRoot(idx) => {
                        emitln!(
                            self.writer,
                            "call {} := $WritebackToValue({}, {}, {});",
                            str_local(*idx),
                            str_local(*src),
                            idx,
                            str_local(*idx)
                        );
                    }
                    Reference(idx) => {
                        emitln!(
                            self.writer,
                            "call {} := $WritebackToReference({}, {});",
                            str_local(*idx),
                            str_local(*src),
                            str_local(*idx)
                        );
                    }
                }
            }
            Splice(_, dest, srcs) => {
                assert!(!srcs.is_empty());
                emitln!(
                    self.writer,
                    "call {} := $Splice{}({}, {});",
                    str_local(*dest),
                    srcs.len(),
                    srcs.iter()
                        .map(|(pos, idx)| format!("{}, {}", pos, str_local(*idx)))
                        .join(", "),
                    str_local(*dest)
                );
            }
            SpecBlock(_, block_id) => {
                self.generate_function_spec_inside_impl(func_target, *block_id);
            }
            Bytecode::Label(_, label) => {
                self.writer.unindent();
                emitln!(self.writer, "L{}:", label.as_usize());
                if ctx.loop_targets.contains_key(label) {
                    emitln!(self.writer, "assume !$abort_flag;");
                    let targets = &ctx.loop_targets[label];
                    for idx in 0..func_target.get_local_count() {
                        if let Some(ref_proxy_idx) = func_target.get_ref_proxy_index(idx) {
                            if targets.contains(ref_proxy_idx) {
                                let ref_proxy_var_name = str_local(*ref_proxy_idx);
                                let proxy_idx = func_target.get_proxy_index(idx).unwrap();
                                emitln!(self.writer,
                            "assume l#$Mutation({}) == $Local({}) && p#$Mutation({}) == $EmptyPath;",
                            ref_proxy_var_name,
                            proxy_idx,
                            ref_proxy_var_name);
                            }
                        }
                        if targets.contains(&idx) {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    str_local(idx).as_str(),
                                    &func_target.get_local_type(idx),
                                    WellFormedMode::Default,
                                )
                            );
                        }
                    }
                }
                self.writer.indent();
            }
            Jump(_, target) => emitln!(self.writer, "goto L{};", target.as_usize()),
            Branch(_, then_target, else_target, idx) => emitln!(
                self.writer,
                "$tmp := {};\nif (b#$Boolean($tmp)) {{ goto L{}; }} else {{ goto L{}; }}",
                str_local(*idx),
                then_target.as_usize(),
                else_target.as_usize(),
            ),
            Assign(_, dest, src, _) => {
                if func_target.get_local_type(*dest).is_reference() {
                    emitln!(
                        self.writer,
                        "call {} := $CopyOrMoveRef({});",
                        str_local(*dest),
                        str_local(*src)
                    );
                    let track = track_local(*dest, str_local(*dest).as_str());
                    if !track.is_empty() {
                        emitln!(self.writer, &track);
                    }
                } else {
                    emitln!(
                        self.writer,
                        "call $tmp := $CopyOrMoveValue({});",
                        str_local(*src)
                    );
                    emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
                }
            }
            Ret(_, rets) => {
                for (i, r) in rets.iter().enumerate() {
                    emitln!(self.writer, "$ret{} := {};", i, str_local(*r));
                    emitln!(self.writer, &track_return(i));
                }
                emitln!(self.writer, "return;");
            }
            Load(_, idx, c) => {
                let value = match c {
                    Constant::Bool(true) => "$Boolean(true)".to_string(),
                    Constant::Bool(false) => "$Boolean(false)".to_string(),
                    Constant::U8(num) => format!("$Integer({})", num),
                    Constant::U64(num) => format!("$Integer({})", num),
                    Constant::U128(num) => format!("$Integer({})", num),
                    Constant::Address(val) => format!("$Address({})", val),
                    Constant::ByteArray(val) => boogie_byte_blob(self.options, val),
                };
                emitln!(self.writer, "$tmp := {};", value);
                emitln!(self.writer, &update_and_track_local(*idx, "$tmp"));
            }
            Call(_, dests, oper, srcs) => {
                use Operation::*;
                match oper {
                    BorrowLoc => {
                        let src = srcs[0];
                        let dest = dests[0];
                        emitln!(
                            self.writer,
                            "call {} := $BorrowLoc({}, {});",
                            str_local(dest),
                            src,
                            str_local(src)
                        );
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    str_local(dest).as_str(),
                                    &func_target.get_local_type(dest),
                                    // At the begining of a borrow, invariant holds.
                                    WellFormedMode::WithInvariant,
                                )
                            );
                        }
                    }
                    ReadRef => {
                        let src = srcs[0];
                        let dest = dests[0];
                        emitln!(self.writer, "call $tmp := $ReadRef({});", str_local(src));
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    "$tmp",
                                    &func_target.get_local_type(dest),
                                    WellFormedMode::Default
                                )
                            );
                        }
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    WriteRef => {
                        let reference = srcs[0];
                        let value = srcs[1];
                        emitln!(
                            self.writer,
                            "call {} := $WriteRef({}, {});",
                            str_local(reference),
                            str_local(reference),
                            str_local(value),
                        );
                        track_mutable_refs(ctx);
                    }
                    FreezeRef => unreachable!(), // eliminated by eliminate_imm_refs
                    Function(mid, fid, type_actuals) => {
                        let callee_env = self.module_env.env.get_module(*mid).into_function(*fid);
                        let callee_target = self.targets.get_target(&callee_env).clone();
                        let entry_point =
                            if self.in_top_level_verify() && !callee_target.is_native() {
                                let inter_module = callee_target.module_env().get_id()
                                    != func_target.module_env().get_id();
                                if inter_module {
                                    FunctionEntryPoint::DirectInterModule
                                } else {
                                    FunctionEntryPoint::DirectIntraModule
                                }
                            } else {
                                FunctionEntryPoint::Indirect
                            };
                        let mut dest_str = String::new();
                        let mut args_str = String::new();
                        let mut dest_type_assumptions = vec![];
                        args_str.push_str(&boogie_type_values(
                            func_target.func_env.module_env.env,
                            type_actuals,
                        ));
                        if !args_str.is_empty() && !srcs.is_empty() {
                            args_str.push_str(", ");
                        }
                        args_str.push_str(
                            &srcs
                                .iter()
                                .map(|arg_idx| format!("{}", str_local(*arg_idx)))
                                .join(", "),
                        );
                        dest_str.push_str(
                            &dests
                                .iter()
                                .map(|dest_idx| {
                                    let dest = str_local(*dest_idx).to_string();
                                    if self.options.prover.assume_wellformed_on_access {
                                        let dest_type = &func_target.get_local_type(*dest_idx);
                                        dest_type_assumptions.push(boogie_well_formed_check(
                                            self.module_env.env,
                                            &dest,
                                            dest_type,
                                            WellFormedMode::Default,
                                        ));
                                    }
                                    dest
                                })
                                .join(", "),
                        );
                        if dest_str == "" {
                            emitln!(
                                self.writer,
                                "call {}{}({});",
                                boogie_function_name(&callee_env),
                                entry_point.suffix(),
                                args_str
                            );
                        } else {
                            emitln!(
                                self.writer,
                                "call {} := {}{}({});",
                                dest_str,
                                boogie_function_name(&callee_env),
                                entry_point.suffix(),
                                args_str
                            );
                        }
                        if callee_env.is_pragma_true(ASSUME_NO_ABORT_FROM_HERE_PRAGMA, || false) {
                            // Assume that calls to this function do not abort
                            emitln!(self.writer, "assume $abort_flag == false;");
                        } else {
                            emitln!(self.writer, &propagate_abort());
                        }
                        for s in &dest_type_assumptions {
                            emitln!(self.writer, s);
                        }
                    }
                    Pack(mid, sid, type_actuals) => {
                        let dest = dests[0];
                        let struct_env = func_target
                            .func_env
                            .module_env
                            .env
                            .get_module(*mid)
                            .into_struct(*sid);
                        let effective_dest = self.compute_effective_dest(func_target, offset, dest);
                        let track_args = if effective_dest < func_target.get_user_local_count() {
                            format!(
                                "{}, {}, {}",
                                func_target
                                    .func_env
                                    .module_env
                                    .env
                                    .file_id_to_idx(loc.file_id()),
                                loc.span().start(),
                                effective_dest,
                            )
                        } else {
                            "0, 0, 0".to_string()
                        };
                        let args_str = type_actuals
                            .iter()
                            .map(|s| boogie_type_value(self.module_env.env, s))
                            .chain(srcs.iter().map(|i| format!("{}", str_local(*i))))
                            .join(", ");
                        emitln!(
                            self.writer,
                            "call $tmp := {}_pack({}, {});",
                            boogie_struct_name(&struct_env),
                            track_args,
                            args_str
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }

                    Unpack(mid, sid, type_actuals) => {
                        let src = srcs[0];
                        let struct_env = func_target
                            .func_env
                            .module_env
                            .env
                            .get_module(*mid)
                            .into_struct(*sid);
                        let mut dests_str = String::new();
                        let mut tmp_assignments = vec![];
                        for dest in dests.iter() {
                            if !dests_str.is_empty() {
                                dests_str.push_str(", ");
                            }
                            let dest_str = str_local(*dest);
                            let dest_type = &func_target.get_local_type(*dest);
                            dests_str.push_str(dest_str.as_str());
                            if !dest_type.is_reference() {
                                tmp_assignments.push(update_and_track_local(*dest, &dest_str));
                            } else {
                                tmp_assignments.push(track_local(*dest, &dest_str));
                            }
                        }
                        let args_str = type_actuals
                            .iter()
                            .map(|s| boogie_type_value(self.module_env.env, s))
                            .chain(vec![format!("{}", str_local(src))])
                            .join(", ");
                        emitln!(
                            self.writer,
                            "call {} := {}_unpack({});",
                            dests_str,
                            boogie_struct_name(&struct_env),
                            args_str,
                        );
                        for s in &tmp_assignments {
                            emitln!(self.writer, s);
                        }
                    }
                    BorrowField(mid, sid, _, field_offset) => {
                        let src = srcs[0];
                        let dest = dests[0];
                        let struct_env = func_target
                            .func_env
                            .module_env
                            .env
                            .get_module(*mid)
                            .into_struct(*sid);
                        let field_env = &struct_env.get_field_by_offset(*field_offset);
                        emitln!(
                            self.writer,
                            "call {} := $BorrowField({}, {});",
                            str_local(dest),
                            str_local(src),
                            boogie_field_name(field_env)
                        );
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    str_local(dest).as_str(),
                                    &func_target.get_local_type(dest),
                                    WellFormedMode::Default
                                )
                            );
                        }
                    }
                    GetField(mid, sid, _, field_offset) => {
                        let src = srcs[0];
                        let dest = dests[0];
                        let struct_env = func_target
                            .func_env
                            .module_env
                            .env
                            .get_module(*mid)
                            .into_struct(*sid);
                        let field_env = &struct_env.get_field_by_offset(*field_offset);
                        emitln!(
                            self.writer,
                            "call $tmp := {}({}, {});",
                            if func_target.get_local_type(src).is_reference() {
                                "$GetFieldFromReference"
                            } else {
                                "$GetFieldFromValue"
                            },
                            str_local(src),
                            boogie_field_name(field_env)
                        );
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    "$tmp",
                                    &func_target.get_local_type(dest),
                                    WellFormedMode::Default
                                )
                            );
                        }
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Exists(mid, sid, type_actuals) => {
                        let addr = srcs[0];
                        let dest = dests[0];
                        let type_args = boogie_type_value_array(self.module_env.env, type_actuals);
                        let memory =
                            boogie_resource_memory_name(self.module_env.env, mid.qualified(*sid));
                        emitln!(
                            self.writer,
                            "$tmp := $ResourceExists({}, {}, {});",
                            memory,
                            type_args,
                            str_local(addr),
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    BorrowGlobal(mid, sid, type_actuals) => {
                        let addr = srcs[0];
                        let dest = dests[0];
                        let type_args = boogie_type_value_array(self.module_env.env, type_actuals);
                        let memory =
                            boogie_resource_memory_name(self.module_env.env, mid.qualified(*sid));
                        emitln!(
                            self.writer,
                            "call {} := $BorrowGlobal({}, {}, {});",
                            str_local(dest),
                            memory,
                            str_local(addr),
                            type_args,
                        );
                        emitln!(self.writer, &propagate_abort());
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    str_local(dest).as_str(),
                                    &func_target.get_local_type(dest),
                                    // At the beginning of a borrow, invariants always hold
                                    WellFormedMode::WithInvariant,
                                )
                            );
                        }
                    }
                    GetGlobal(mid, sid, type_actuals) => {
                        let addr = srcs[0];
                        let dest = dests[0];
                        let type_args = boogie_type_value_array(self.module_env.env, type_actuals);
                        let memory =
                            boogie_resource_memory_name(self.module_env.env, mid.qualified(*sid));
                        emitln!(
                            self.writer,
                            "call $tmp := $GetGlobal({}, {}, {});",
                            memory,
                            str_local(addr),
                            type_args,
                        );
                        emitln!(self.writer, &propagate_abort());
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    "$tmp",
                                    &func_target.get_local_type(dest),
                                    // At the beginning of a borrow, invariants always hold
                                    WellFormedMode::WithInvariant,
                                )
                            );
                        }
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    MoveTo(mid, sid, type_actuals) => {
                        let value = srcs[0];
                        let signer = srcs[1];
                        let type_args = boogie_type_value_array(self.module_env.env, type_actuals);
                        let memory = mid.qualified(*sid);
                        let spec_translator = self.new_spec_translator_for_module();
                        spec_translator.save_memory_for_update_invariants(memory);
                        let memory_name = boogie_resource_memory_name(self.module_env.env, memory);
                        emitln!(
                            self.writer,
                            "call {} := $MoveTo({}, {}, {}, {});",
                            memory_name,
                            memory_name,
                            type_args,
                            str_local(value),
                            str_local(signer),
                        );
                        emitln!(self.writer, &propagate_abort());
                        spec_translator.emit_global_invariants_for_memory(
                            false, // assert
                            memory,
                        );
                    }
                    MoveFrom(mid, sid, type_actuals) => {
                        let src = srcs[0];
                        let dest = dests[0];
                        let type_args = boogie_type_value_array(self.module_env.env, type_actuals);
                        let memory = mid.qualified(*sid);
                        let spec_translator = self.new_spec_translator_for_module();
                        spec_translator.save_memory_for_update_invariants(memory);
                        let memory_name = boogie_resource_memory_name(self.module_env.env, memory);
                        emitln!(
                            self.writer,
                            "call {}, $tmp := $MoveFrom({}, {}, {});",
                            memory_name,
                            memory_name,
                            str_local(src),
                            type_args,
                        );
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                        if self.options.prover.assume_wellformed_on_access {
                            emit!(
                                self.writer,
                                &boogie_well_formed_check(
                                    self.module_env.env,
                                    "$tmp",
                                    &func_target.get_local_type(dest),
                                    WellFormedMode::Default
                                )
                            );
                        }
                        spec_translator.emit_global_invariants_for_memory(
                            false, // assert
                            memory,
                        );
                    }
                    CastU8 => {
                        let src = srcs[0];
                        let dest = dests[0];
                        emitln!(self.writer, "call $tmp := $CastU8({});", str_local(src));
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    CastU64 => {
                        let src = srcs[0];
                        let dest = dests[0];
                        emitln!(self.writer, "call $tmp := $CastU64({});", str_local(src));
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    CastU128 => {
                        let src = srcs[0];
                        let dest = dests[0];
                        emitln!(self.writer, "call $tmp := $CastU128({});", str_local(src));
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Not => {
                        let src = srcs[0];
                        let dest = dests[0];
                        emitln!(self.writer, "call $tmp := $Not({});", str_local(src));
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Add => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        let unchecked = if func_target
                            .is_pragma_true(ADDITION_OVERFLOW_UNCHECKED_PRAGMA, || false)
                        {
                            "_unchecked"
                        } else {
                            ""
                        };
                        let add_type = match func_target.get_local_type(dest) {
                            Type::Primitive(PrimitiveType::U8) => "U8".to_string(),
                            Type::Primitive(PrimitiveType::U64) => format!("U64{}", unchecked),
                            Type::Primitive(PrimitiveType::U128) => format!("U128{}", unchecked),
                            _ => unreachable!(),
                        };
                        emitln!(
                            self.writer,
                            "call $tmp := $Add{}({}, {});",
                            add_type,
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Sub => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Sub({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Mul => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        let mul_type = match func_target.get_local_type(dest) {
                            Type::Primitive(PrimitiveType::U8) => "U8",
                            Type::Primitive(PrimitiveType::U64) => "U64",
                            Type::Primitive(PrimitiveType::U128) => "U128",
                            _ => unreachable!(),
                        };
                        emitln!(
                            self.writer,
                            "call $tmp := $Mul{}({}, {});",
                            mul_type,
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Div => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Div({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Mod => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Mod({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &propagate_abort());
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Shl => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Shl({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Shr => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Shr({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Lt => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Lt({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Gt => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Gt({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Le => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Le({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Ge => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Ge({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Or => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $Or({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    And => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "call $tmp := $And({}, {});",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Eq => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "$tmp := $Boolean($IsEqual({}, {}));",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    Neq => {
                        let dest = dests[0];
                        let op1 = srcs[0];
                        let op2 = srcs[1];
                        emitln!(
                            self.writer,
                            "$tmp := $Boolean(!$IsEqual({}, {}));",
                            str_local(op1),
                            str_local(op2)
                        );
                        emitln!(self.writer, &update_and_track_local(dest, "$tmp"));
                    }
                    BitOr | BitAnd | Xor => {
                        emitln!(
                            self.writer,
                            "// bit operation not supported: {:?}\nassert false;",
                            bytecode
                        );
                    }
                    Destroy => {}
                }
            }
            Abort(..) => {
                // Below we introduce a dummy `if` for $DebugTrackAbort to ensure boogie creates
                // a execution trace entry for this statement.
                emitln!(
                    self.writer,
                    "if (true) {{ assume $DebugTrackAbort({}, {}); }}",
                    func_target
                        .func_env
                        .module_env
                        .env
                        .file_id_to_idx(loc.file_id()),
                    loc.span().start(),
                );
                emitln!(self.writer, "goto Abort;")
            }
            Nop(..) => {}
        }

        emitln!(self.writer);
    }

    // Compute effective destination to enhance debug experience. This looks ahead
    // at the next instructions to detect a simple aliasing via copy or move and use that
    // destination as it might be a user variable whereas this instruction has a temporary
    // destination introduced by stackless bytecode transformation.
    // TODO: the stackless bytecode should optimize away unnecessary copy/moves, so we
    // don't need this. The below transformation is only correct for stackless code
    // of certain shape
    fn compute_effective_dest(
        &self,
        func_target: &FunctionTarget<'_>,
        offset: CodeOffset,
        dest: usize,
    ) -> usize {
        let code = func_target.get_bytecode();
        if dest >= func_target.get_local_count() && offset as usize + 1 < code.len() {
            if let Call(_, temp_dests, Operation::Pack(..), ..) = &code[offset as usize] {
                if let Assign(_, effective_dest, src, _) = &code[offset as usize + 1] {
                    if *src == temp_dests[0] {
                        return *effective_dest;
                    }
                }
            }
        }
        dest
    }

    /// If ty is a mutable reference to a struct, return its environment.
    fn get_referred_struct(&self, ty: &Type) -> Option<(StructEnv<'_>, Vec<Type>)> {
        if let Type::Reference(true, bt) = &ty {
            if let Type::Struct(module_idx, struct_idx, type_args) = bt.as_ref() {
                return Some((
                    self.module_env
                        .env
                        .get_module(*module_idx)
                        .into_struct(*struct_idx),
                    type_args.clone(),
                ));
            }
        }
        None
    }

    /// Enforce the invariant of an updated value before mutation starts. Does nothing if there
    /// is no before-update invariant.
    fn enforce_before_update_invariant(&self, func_target: &FunctionTarget<'_>, idx: usize) {
        if let Some((struct_env, type_args)) =
            self.get_referred_struct(func_target.get_local_type(idx))
        {
            if SpecTranslator::has_before_update_invariant(&struct_env) {
                let name = func_target
                    .symbol_pool()
                    .string(func_target.get_local_name(idx));
                let args_str = type_args
                    .iter()
                    .map(|ty| boogie_type_value(self.module_env.env, ty))
                    .chain(vec![format!("$Dereference({})", name)])
                    .join(", ");
                emitln!(
                    self.writer,
                    "call {}_before_update_inv({});",
                    boogie_struct_name(&struct_env),
                    args_str,
                );
            }
        }
    }

    /// Enforce the invariant of an updated value after mutation ended. Does nothing if there is
    /// no after-update invariant.
    fn enforce_after_update_invariant(&self, func_target: &FunctionTarget<'_>, idx: usize) {
        if let Some((struct_env, type_args)) =
            self.get_referred_struct(func_target.get_local_type(idx))
        {
            if SpecTranslator::has_after_update_invariant(&struct_env) {
                let name = func_target
                    .symbol_pool()
                    .string(func_target.get_local_name(idx));
                let args_str = type_args
                    .iter()
                    .map(|ty| boogie_type_value(self.module_env.env, ty))
                    .chain(vec![format!("$Dereference({})", name)])
                    .join(", ");
                emitln!(
                    self.writer,
                    "call {}_after_update_inv({});",
                    boogie_struct_name(&struct_env),
                    args_str,
                );
            }
        }
    }

    /// Updates a local, injecting debug information if available.
    fn update_and_track_local(
        &self,
        func_target: &FunctionTarget<'_>,
        loc: Loc,
        idx: usize,
        value: &str,
    ) -> String {
        let name = func_target
            .symbol_pool()
            .string(func_target.get_local_name(idx));
        let update = format!("{} := {};", name, value);
        let debug_update = self.track_local(func_target, loc, idx, value);
        if !debug_update.is_empty() {
            format!("{}\n{}", update, debug_update)
        } else {
            update
        }
    }

    /// Generates an update of the model debug variable at given location.
    fn track_local(
        &self,
        func_target: &FunctionTarget<'_>,
        loc: Loc,
        idx: usize,
        value: &str,
    ) -> String {
        // Check whether this is a temporary, which we do not want to track. Indices >=
        // local_count are return values which we do track.
        if idx >= func_target.get_user_local_count() && idx < func_target.get_local_count() {
            return "".to_string();
        }
        let ty = if idx < func_target.get_local_count() {
            func_target.get_local_type(idx)
        } else {
            func_target.get_return_type(idx - func_target.get_local_count())
        };
        let value = if ty.is_reference() {
            format!("$Dereference({})", value)
        } else {
            value.to_string()
        };
        format!(
            "if (true) {{ assume $DebugTrackLocal({}, {}, {}, {}); }}",
            func_target
                .func_env
                .module_env
                .env
                .file_id_to_idx(loc.file_id()),
            loc.span().start(),
            idx,
            value
        )
    }
}

/// Separates elements in vector, dropping empty ones.
fn separate(elems: Vec<String>, sep: &str) -> String {
    elems.iter().filter(|s| !s.is_empty()).join(sep)
}
