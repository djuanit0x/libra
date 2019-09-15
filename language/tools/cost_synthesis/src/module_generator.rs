// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Logic for random valid module and module universe generation.
//!
//! This module contains the logic for generating random valid modules and valid (rooted) module
//! universes. Note that we do not generate valid function bodies for the functions that are
//! generated -- any function bodies that are generated are simply non-semantic sequences of
//! instructions to check BrTrue, BrFalse, and Branch instructions.
use crate::common::*;
use bytecode_verifier::VerifiedModule;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{cmp::min, collections::HashMap};
use types::{
    account_address::AccountAddress, byte_array::ByteArray, identifier::Identifier,
    language_storage::ModuleId,
};
use vm::{
    access::*,
    file_format::{
        AddressPoolIndex, Bytecode, CodeUnit, CompiledModule, CompiledModuleMut, FieldDefinition,
        FieldDefinitionIndex, FunctionDefinition, FunctionHandle, FunctionHandleIndex,
        FunctionSignature, FunctionSignatureIndex, IdentifierIndex, LocalsSignature,
        LocalsSignatureIndex, MemberCount, ModuleHandle, ModuleHandleIndex, SignatureToken,
        StructDefinition, StructFieldInformation, StructHandle, StructHandleIndex, TableIndex,
        TypeSignature, TypeSignatureIndex,
    },
    internals::ModuleIndex,
};

type BytecodeGenerator =
    dyn Fn(&[SignatureToken], &FunctionSignature, CompiledModuleMut) -> Vec<Bytecode>;

/// A wrapper around a `CompiledModule` containing information needed for generation.
///
/// Contains a source of pseudo-randomness along with a table of the modules that are known and can
/// be called into -- these are all modules that have previously been generated by the same
/// instance of the `ModuleBuilder`.
///
/// The call graph between generated modules forms a rooted DAG based at the current
/// `CompiledModule` being generated.
pub struct ModuleBuilder {
    /// The source of randomness used across the modules that we generate.
    gen: StdRng,

    /// The current module being built.
    module: CompiledModuleMut,

    /// The minimum size of the tables in the generated module.
    table_size: TableIndex,

    /// Other modules that we know, and that we can generate calls type references into. Indexed by
    /// their address and name (i.e. the module's `ModuleId`).
    known_modules: HashMap<ModuleId, CompiledModule>,

    /// Bytecode generation for function bodies
    bytecode_gen: Option<Box<BytecodeGenerator>>,
}

impl ModuleBuilder {
    /// Create a new module builder with generated module tables of size `table_size`.
    pub fn new(table_size: TableIndex, bytecode_gen: Option<Box<BytecodeGenerator>>) -> Self {
        let seed: [u8; 32] = [0; 32];
        Self {
            gen: StdRng::from_seed(seed),
            module: Self::default_module_with_types(),
            table_size,
            known_modules: HashMap::new(),
            bytecode_gen,
        }
    }

    /// Display the current module being generated.
    pub fn display(&self) {
        println!("{:#?}", self.module)
    }

    fn with_account_addresses(&mut self) {
        let mut addrs = (0..self.table_size)
            .map(|_| AccountAddress::random())
            .collect();
        self.module.address_pool.append(&mut addrs);
    }

    fn with_identifiers(&mut self) {
        let mut identifiers = (0..self.table_size)
            .map(|_| {
                let len = self.gen.gen_range(1, MAX_STRING_SIZE);
                // TODO: restrict identifiers to a subset of ASCII
                let s: String = (0..len).map(|_| self.gen.gen::<char>()).collect();
                Identifier::new(s).unwrap()
            })
            .collect();
        self.module.identifiers.append(&mut identifiers);
    }

    fn with_user_strings(&mut self) {
        let mut strs = (0..self.table_size)
            .map(|_| {
                let len = self.gen.gen_range(1, MAX_STRING_SIZE);
                (0..len)
                    .map(|_| self.gen.gen::<char>())
                    .collect::<String>()
                    .into()
            })
            .collect();
        self.module.user_strings.append(&mut strs);
    }

    fn with_bytearrays(&mut self) {
        self.module.byte_array_pool = (0..self.table_size)
            .map(|_| {
                let len = self.gen.gen_range(1, BYTE_ARRAY_MAX_SIZE);
                let bytes = (0..len).map(|_| self.gen.gen::<u8>()).collect();
                ByteArray::new(bytes)
            })
            .collect();
    }

    // Add the functions with locals given by the first part of the tuple, and with function
    // signature `FunctionSignature`.
    fn with_functions(&mut self, sigs: Vec<(Vec<SignatureToken>, FunctionSignature)>) {
        let mut names: Vec<Identifier> = sigs
            .iter()
            .enumerate()
            .map(|(i, _)| Identifier::new(format!("func{}", i)).unwrap())
            .collect();
        // Grab the offset before adding the generated names to the string pool; we'll need this
        // later on when we generate the function handles in order to know where we should have the
        // functions point to for their name.
        let offset = self.module.identifiers.len();
        let function_sig_offset = self.module.function_signatures.len();
        self.module.identifiers.append(&mut names);

        self.module.function_handles = sigs
            .iter()
            .enumerate()
            .map(|(i, _)| FunctionHandle {
                name: IdentifierIndex::new((i + offset) as u16),
                signature: FunctionSignatureIndex::new((i + function_sig_offset) as u16),
                module: ModuleHandleIndex::new(0),
            })
            .collect();
        let (local_sigs, mut function_sigs): (Vec<_>, Vec<_>) = sigs.clone().into_iter().unzip();
        self.module.function_signatures.append(&mut function_sigs);
        self.module
            .locals_signatures
            .append(&mut local_sigs.into_iter().map(LocalsSignature).collect());

        self.module.function_defs = sigs
            .iter()
            .enumerate()
            .map(|(i, sig)| FunctionDefinition {
                function: FunctionHandleIndex::new(i as u16),
                flags: CodeUnit::PUBLIC,
                // TODO this needs to be generated
                acquires_global_resources: vec![],
                code: CodeUnit {
                    max_stack_size: 20,
                    locals: LocalsSignatureIndex(i as u16),
                    code: {
                        match &self.bytecode_gen {
                            Some(bytecode_gen) => bytecode_gen(&sig.0, &sig.1, self.module.clone()),
                            None => {
                                // Random nonsense to pad this out. We won't look at this at all,
                                // just non-empty is all that matters.
                                vec![Bytecode::Sub, Bytecode::Sub, Bytecode::Add, Bytecode::Ret]
                            }
                        }
                    },
                },
            })
            .collect();
    }

    // Generate `table_size` number of structs. Note that this will not generate nested structs.
    // The overall logic of this function follows very similarly to that for function generation.
    fn with_structs(&mut self) {
        // Generate struct names.
        let mut names: Vec<Identifier> = (0..self.table_size)
            .map(|i| Identifier::new(format!("struct{}", i)).unwrap())
            .collect();
        let offset = self.module.identifiers.len() as TableIndex;
        self.module.identifiers.append(&mut names);

        // Generate the field definitions and struct definitions at the same time
        for struct_idx in 0..self.table_size {
            // Generate a random amount of fields for each struct. Each struct must have at least
            // one field.
            let num_fields = self
                .gen
                .gen_range(1, min(self.module.identifiers.len(), MAX_FIELDS));

            // Generate the struct def. This generates pointers into the module's `field_defs` that
            // are not generated just yet -- we do this beforehand so that we can grab the starting
            // index into the module's `field_defs` table before we generate the struct's fields.
            let field_information = StructFieldInformation::Declared {
                field_count: num_fields as MemberCount,
                fields: FieldDefinitionIndex::new(self.module.field_defs.len() as TableIndex),
            };
            let struct_def = StructDefinition {
                struct_handle: StructHandleIndex(struct_idx),
                field_information,
            };
            self.module.struct_defs.push(struct_def);

            // Generate the fields for the struct.
            for i in 0..num_fields {
                let struct_handle_idx = StructHandleIndex::new(struct_idx);
                // Pick a random base type (non-reference)
                let typ_idx = TypeSignatureIndex::new(
                    self.gen
                        .gen_range(0, self.module.type_signatures.len() as TableIndex),
                );
                // Pick a name from the string pool.
                let str_pool_idx = IdentifierIndex::new(i as TableIndex);
                let field_def = FieldDefinition {
                    struct_: struct_handle_idx,
                    name: str_pool_idx,
                    signature: typ_idx,
                };
                self.module.field_defs.push(field_def);
            }
        }

        // Generate the struct handles. This needs to be in sync with the names that we generated
        // earlier at the start of this function.
        self.module.struct_handles = (0..self.table_size)
            .map(|struct_idx| StructHandle {
                module: ModuleHandleIndex::new(0),
                name: IdentifierIndex::new((struct_idx + offset) as TableIndex),
                is_nominal_resource: self.gen.gen_bool(1.0 / 2.0),
                type_formals: vec![],
            })
            .collect();
    }

    // Generate `table_size` number of functions in the underlying module. This does this by
    // generating a bunch of random locals type signatures (Vec<SignatureToken>) and the
    // FunctionSignatures. We then call `with_functions` with this generated type info.
    fn with_random_functions(&mut self) {
        use SignatureToken::*;
        // The base signature tokens that we can use for our types.
        let sig_toks = vec![Bool, U64, String, ByteArray, Address];
        // Generate a bunch of random function signatures over these types.
        let functions = (0..self.table_size)
            .map(|_| {
                let num_locals = self.gen.gen_range(1, MAX_NUM_LOCALS);
                let num_args = self.gen.gen_range(1, MAX_FUNCTION_CALL_SIZE);
                let num_return_types = self.gen.gen_range(1, MAX_RETURN_TYPES_LENGTH);

                let locals = (0..num_locals)
                    .map(|_| {
                        let index = self.gen.gen_range(0, sig_toks.len());
                        sig_toks[index].clone()
                    })
                    .collect();

                let args = (0..num_args)
                    .map(|_| {
                        let index = self.gen.gen_range(0, sig_toks.len());
                        sig_toks[index].clone()
                    })
                    .collect();

                let return_types = (0..num_return_types)
                    .map(|_| {
                        let index = self.gen.gen_range(0, sig_toks.len());
                        sig_toks[index].clone()
                    })
                    .collect();

                // Generate the function signature. We don't care about the return type of the
                // function, so we don't generate any types, and default to saying that it returns
                // the unit type.
                let function_sig = FunctionSignature {
                    arg_types: args,
                    return_types,
                    type_formals: vec![],
                };

                (locals, function_sig)
            })
            .collect();

        self.with_cross_calls();
        self.with_functions(functions);
    }

    fn with_cross_calls(&mut self) {
        let module_table_size = self.module.module_handles.len();
        if module_table_size < 2 {
            return;
        }

        // We have half/half inter- and intra-module calls.
        let number_of_cross_calls = self.table_size;
        for _ in 0..number_of_cross_calls {
            let non_self_module_handle_idx = self.gen.gen_range(1, module_table_size);
            let callee_module_handle = &self.module.module_handles[non_self_module_handle_idx];
            let address = self.module.address_pool[callee_module_handle.address.into_index()];
            let name = &self.module.identifiers[callee_module_handle.name.into_index()];
            let module_id = ModuleId::new(address, name.to_owned());
            let callee_module = self
                .known_modules
                .get(&module_id)
                .expect("[Module Lookup] Unable to get module from known_modules.");

            let callee_function_handle_idx = self
                .gen
                .gen_range(0, callee_module.function_handles().len())
                as TableIndex;
            let callee_function_handle = callee_module
                .function_handle_at(FunctionHandleIndex::new(callee_function_handle_idx));
            let callee_type_sig = callee_module
                .function_signature_at(callee_function_handle.signature)
                .clone();
            let callee_name = callee_module
                .identifier_at(callee_function_handle.name)
                .to_owned();
            let callee_name_idx = self.module.identifiers.len() as TableIndex;
            let callee_type_sig_idx = self.module.function_signatures.len() as TableIndex;
            let func_handle = FunctionHandle {
                module: ModuleHandleIndex::new(non_self_module_handle_idx as TableIndex),
                name: IdentifierIndex::new(callee_name_idx),
                signature: FunctionSignatureIndex::new(callee_type_sig_idx),
            };

            self.module.identifiers.push(callee_name);
            self.module.function_signatures.push(callee_type_sig);
            self.module.function_handles.push(func_handle);
        }
    }

    // Add the modules identitied by their code keys to the module handles of the underlying
    // CompiledModule.
    fn with_callee_modules(&mut self) {
        // Add the SELF module
        let module_name: String = (0..10).map(|_| self.gen.gen::<char>()).collect();
        let module_name = Identifier::new(module_name).unwrap();
        self.module.identifiers.insert(0, module_name);
        self.module.address_pool.insert(0, AccountAddress::random());
        // Recall that we inserted the module name at index 0 in the string pool.
        let self_module_handle = ModuleHandle {
            address: AddressPoolIndex::new(0),
            name: IdentifierIndex::new(0),
        };
        self.module.module_handles.insert(0, self_module_handle);

        let (mut names, mut addresses) = self
            .known_modules
            .keys()
            .map(|key| (key.name().into(), key.address()))
            .unzip();

        let address_pool_offset = self.module.address_pool.len() as TableIndex;
        let identifier_offset = self.module.identifiers.len() as TableIndex;
        // Add the strings and addresses to the pool
        self.module.identifiers.append(&mut names);
        self.module.address_pool.append(&mut addresses);

        let mut module_handles = (0..self.known_modules.len())
            .map(|i| {
                let i = i as TableIndex;
                ModuleHandle {
                    address: AddressPoolIndex::new(address_pool_offset + i),
                    name: IdentifierIndex::new(identifier_offset + i),
                }
            })
            .collect();
        self.module.module_handles.append(&mut module_handles);
    }

    /// This method builds and then materializes the underlying module skeleton. It then swaps in a
    /// new module skeleton, adds the generated module to the `known_modules`, and returns
    /// the generated module.
    pub fn materialize_unverified(&mut self) -> CompiledModule {
        self.with_callee_modules();
        self.with_account_addresses();
        self.with_identifiers();
        self.with_user_strings();
        self.with_bytearrays();
        self.with_structs();
        self.with_random_functions();
        let module = std::mem::replace(&mut self.module, Self::default_module_with_types());
        let module = module.freeze().expect("should satisfy bounds checker");
        self.known_modules.insert(module.self_id(), module.clone());
        // We don't expect the module to pass the verifier at the moment. This is OK because it
        // isn't part of the core code path, just something done to the side.
        module
    }

    /// This method builds and then materializes the underlying module skeleton. It then swaps in a
    /// new module skeleton, adds the generated module to the `known_modules`, and returns
    /// the generated module as a Verified Module.
    pub fn materialize(&mut self) -> VerifiedModule {
        let module = self.materialize_unverified();
        VerifiedModule::bypass_verifier_DANGEROUS_FOR_TESTING_ONLY(module)
    }

    // This method generates a default (empty) `CompiledModuleMut` but with base types. This way we
    // can point to them when generating structs/functions etc.
    fn default_module_with_types() -> CompiledModuleMut {
        use SignatureToken::*;
        let mut module = CompiledModuleMut::default();
        module.type_signatures = vec![Bool, U64, String, ByteArray, Address]
            .into_iter()
            .map(TypeSignature)
            .collect();
        module
    }
}

/// A wrapper around a `ModuleBuilder` for building module universes.
///
/// The `ModuleBuilder` is already designed to build module universes but the size of this universe
/// is unspecified and un-iterable. This is a simple wrapper around the builder that allows
/// the implementation of the `Iterator` trait over it.
pub struct ModuleGenerator {
    module_builder: ModuleBuilder,
    iters: u64,
}

impl ModuleGenerator {
    /// Create a new `ModuleGenerator` where each generated module has at least `table_size`
    /// elements in each table, and where `iters` many modules are generated.
    pub fn new(table_size: TableIndex, iters: u64) -> Self {
        Self {
            module_builder: ModuleBuilder::new(table_size, None),
            iters,
        }
    }
}

impl Iterator for ModuleGenerator {
    type Item = VerifiedModule;
    fn next(&mut self) -> Option<Self::Item> {
        if self.iters == 0 {
            None
        } else {
            self.iters -= 1;
            Some(self.module_builder.materialize())
        }
    }
}
