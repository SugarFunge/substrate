// This file is part of Substrate.

// Copyright (C) 2018-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! This module implements sandboxing support in the runtime.
//!
//! Sandboxing is baked by wasmi at the moment. In future, however, we would like to add/switch to
//! a compiled execution engine.

use crate::error::{Result, Error};
use std::{cell::RefCell, collections::HashMap, convert::TryInto, rc::Rc, todo};
use codec::{Decode, Encode};
use sp_core::sandbox as sandbox_primitives;

use wasmi::{Externals, ImportResolver, MemoryDescriptor, MemoryInstance, MemoryRef, Module, ModuleInstance, RuntimeArgs, RuntimeValue, Trap, TrapKind, memory_units::Pages};

use sp_wasm_interface::Value;
use wasmtime::Val;

// use crate::sandbox:: wasmtime::instance_wrapper::InstanceWrapper;

// use wasmtime::{Store, Instance, Module, Memory, Table, Val, Func, Extern, Global};

use sp_wasm_interface::{FunctionContext, Pointer, WordSize};

// use wasmer::{imports, wat2wasm, Instance, Module, Store, Value};
use wasmer_compiler_singlepass::{Singlepass, SinglepassCompiler};

/// Index of a function inside the supervisor.
///
/// This is a typically an index in the default table of the supervisor, however
/// the exact meaning of this index is depends on the implementation of dispatch function.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SupervisorFuncIndex(usize);

impl From<SupervisorFuncIndex> for usize {
	fn from(index: SupervisorFuncIndex) -> Self {
		index.0
	}
}

/// Index of a function within guest index space.
///
/// This index is supposed to be used as index for `Externals`.
#[derive(Copy, Clone, Debug, PartialEq)]
struct GuestFuncIndex(usize);

/// This struct holds a mapping from guest index space to supervisor.
struct GuestToSupervisorFunctionMapping {
	/// Position of elements in this vector are interpreted
	/// as indices of guest functions and are mappeed to
	/// corresponding supervisor function indices.
	funcs: Vec<SupervisorFuncIndex>,
}

impl GuestToSupervisorFunctionMapping {
	/// Create an empty function mapping
	fn new() -> GuestToSupervisorFunctionMapping {
		GuestToSupervisorFunctionMapping { funcs: Vec::new() }
	}

	/// Add a new supervisor function to the mapping.
	/// Returns a newly assigned guest function index.
	fn define(&mut self, supervisor_func: SupervisorFuncIndex) -> GuestFuncIndex {
		let idx = self.funcs.len();
		self.funcs.push(supervisor_func);
		GuestFuncIndex(idx)
	}

	/// Find supervisor function index by its corresponding guest function index
	fn func_by_guest_index(&self, guest_func_idx: GuestFuncIndex) -> Option<SupervisorFuncIndex> {
		self.funcs.get(guest_func_idx.0).cloned()
	}
}

/// Holds sandbox function and memory imports and performs name resolution
struct Imports {
	/// Maps qualified function name to its guest function index
	func_map: HashMap<(Vec<u8>, Vec<u8>), GuestFuncIndex>,

	/// Maps qualified field name to its memory reference
	memories_map: HashMap<(Vec<u8>, Vec<u8>), Memory>,
}

impl Imports {
	fn func_by_name(&self, module_name: &str, func_name: &str) -> Option<GuestFuncIndex> {
		self.func_map.get(&(module_name.as_bytes().to_owned(), func_name.as_bytes().to_owned())).cloned()
	}

	fn memory_by_name(&self, module_name: &str, memory_name: &str) -> Option<Memory> {
		self.memories_map.get(&(module_name.as_bytes().to_owned(), memory_name.as_bytes().to_owned())).cloned()
	}
}

impl ImportResolver for Imports {
	fn resolve_func(
		&self,
		module_name: &str,
		field_name: &str,
		signature: &::wasmi::Signature,
	) -> std::result::Result<wasmi::FuncRef, wasmi::Error> {
		let key = (
			module_name.as_bytes().to_owned(),
			field_name.as_bytes().to_owned(),
		);
		let idx = *self.func_map.get(&key).ok_or_else(|| {
			wasmi::Error::Instantiation(format!(
				"Export {}:{} not found",
				module_name, field_name
			))
		})?;
		Ok(wasmi::FuncInstance::alloc_host(signature.clone(), idx.0))
	}

	fn resolve_memory(
		&self,
		module_name: &str,
		field_name: &str,
		_memory_type: &::wasmi::MemoryDescriptor,
	) -> std::result::Result<MemoryRef, wasmi::Error> {
		let key = (
			module_name.as_bytes().to_vec(),
			field_name.as_bytes().to_vec(),
		);
		let mem = self.memories_map
			.get(&key)
			.and_then(|m| m.as_wasmi())
			.ok_or_else(|| {
				wasmi::Error::Instantiation(format!(
					"Export {}:{} not found",
					module_name, field_name
				))
			})?
			.clone();
		Ok(mem)
	}

	fn resolve_global(
		&self,
		module_name: &str,
		field_name: &str,
		_global_type: &::wasmi::GlobalDescriptor,
	) -> std::result::Result<wasmi::GlobalRef, wasmi::Error> {
		Err(wasmi::Error::Instantiation(format!(
			"Export {}:{} not found",
			module_name, field_name
		)))
	}

	fn resolve_table(
		&self,
		module_name: &str,
		field_name: &str,
		_table_type: &::wasmi::TableDescriptor,
	) -> std::result::Result<wasmi::TableRef, wasmi::Error> {
		Err(wasmi::Error::Instantiation(format!(
			"Export {}:{} not found",
			module_name, field_name
		)))
	}
}

/// This trait encapsulates sandboxing capabilities.
///
/// Note that this functions are only called in the `supervisor` context.
pub trait SandboxCapabilities: FunctionContext {
	/// Represents a function reference into the supervisor environment.
	/// Provides an abstraction over execution environment.
	type SupervisorFuncRef;

	/// Invoke a function in the supervisor environment.
	///
	/// This first invokes the dispatch_thunk function, passing in the function index of the
	/// desired function to call and serialized arguments. The thunk calls the desired function
	/// with the deserialized arguments, then serializes the result into memory and returns
	/// reference. The pointer to and length of the result in linear memory is encoded into an `i64`,
	/// with the upper 32 bits representing the pointer and the lower 32 bits representing the
	/// length.
	///
	/// # Errors
	///
	/// Returns `Err` if the dispatch_thunk function has an incorrect signature or traps during
	/// execution.
	fn invoke(
		&mut self,
		dispatch_thunk: &Self::SupervisorFuncRef,
		invoke_args_ptr: Pointer<u8>,
		invoke_args_len: WordSize,
		state: u32,
		func_idx: SupervisorFuncIndex,
	) -> Result<i64>;
}

/// Implementation of [`Externals`] that allows execution of guest module with
/// [externals][`Externals`] that might refer functions defined by supervisor.
///
/// [`Externals`]: ../wasmi/trait.Externals.html
pub struct GuestExternals<'a, FE: SandboxCapabilities + 'a> {
	/// Supervisor function environment
	supervisor_externals: &'a mut FE,

	/// Instance of sandboxed module to be dispatched
	sandbox_instance: &'a SandboxInstance<FE::SupervisorFuncRef>,

	/// Opaque pointer to outer context, see the `instantiate` function
	state: u32,
}

/// Construct trap error from specified message
fn trap(msg: &'static str) -> Trap {
	TrapKind::Host(Box::new(Error::Other(msg.into()))).into()
}

/// Deserialize bytes into `Result`
fn deserialize_result(serialized_result: &[u8]) -> std::result::Result<Option<RuntimeValue>, Trap> {
	use self::sandbox_primitives::HostError;
	use sp_wasm_interface::ReturnValue;
	let result_val = std::result::Result::<ReturnValue, HostError>::decode(&mut &serialized_result[..])
		.map_err(|_| trap("Decoding Result<ReturnValue, HostError> failed!"))?;

	match result_val {
		Ok(return_value) => Ok(match return_value {
			ReturnValue::Unit => None,
			ReturnValue::Value(typed_value) => Some(RuntimeValue::from(typed_value)),
		}),
		Err(HostError) => Err(trap("Supervisor function returned sandbox::HostError")),
	}
}

impl<'a, FE: SandboxCapabilities + 'a> Externals for GuestExternals<'a, FE> {
	fn invoke_index(
		&mut self,
		index: usize,
		args: RuntimeArgs,
	) -> std::result::Result<Option<RuntimeValue>, Trap> {
		// Make `index` typesafe again.
		let index = GuestFuncIndex(index);

		// Convert function index from guest to supervisor space
		let func_idx = self.sandbox_instance
			.guest_to_supervisor_mapping
			.func_by_guest_index(index)
			.expect(
				"`invoke_index` is called with indexes registered via `FuncInstance::alloc_host`;
					`FuncInstance::alloc_host` is called with indexes that was obtained from `guest_to_supervisor_mapping`;
					`func_by_guest_index` called with `index` can't return `None`;
					qed"
			);

		// Serialize arguments into a byte vector.
		let invoke_args_data: Vec<u8> = args.as_ref()
			.iter()
			.cloned()
			.map(sp_wasm_interface::Value::from)
			.collect::<Vec<_>>()
			.encode();

		let state = self.state;

		// Move serialized arguments inside the memory, invoke dispatch thunk and
		// then free allocated memory.
		let invoke_args_len = invoke_args_data.len() as WordSize;
		let invoke_args_ptr = self
			.supervisor_externals
			.allocate_memory(invoke_args_len)
			.map_err(|_| trap("Can't allocate memory in supervisor for the arguments"))?;

		let deallocate = |this: &mut GuestExternals<FE>, ptr, fail_msg| {
			this
				.supervisor_externals
				.deallocate_memory(ptr)
				.map_err(|_| trap(fail_msg))
		};

		if self
			.supervisor_externals
			.write_memory(invoke_args_ptr, &invoke_args_data)
			.is_err()
		{
			deallocate(self, invoke_args_ptr, "Failed dealloction after failed write of invoke arguments")?;
			return Err(trap("Can't write invoke args into memory"));
		}

		let result = self.supervisor_externals.invoke(
			&self.sandbox_instance.dispatch_thunk,
			invoke_args_ptr,
			invoke_args_len,
			state,
			func_idx,
		);

		deallocate(self, invoke_args_ptr, "Can't deallocate memory for dispatch thunk's invoke arguments")?;
		let result = result?;

		// dispatch_thunk returns pointer to serialized arguments.
		// Unpack pointer and len of the serialized result data.
		let (serialized_result_val_ptr, serialized_result_val_len) = {
			// Cast to u64 to use zero-extension.
			let v = result as u64;
			let ptr = (v as u64 >> 32) as u32;
			let len = (v & 0xFFFFFFFF) as u32;
			(Pointer::new(ptr), len)
		};

		let serialized_result_val = self.supervisor_externals
			.read_memory(serialized_result_val_ptr, serialized_result_val_len)
			.map_err(|_| trap("Can't read the serialized result from dispatch thunk"));

		deallocate(self, serialized_result_val_ptr, "Can't deallocate memory for dispatch thunk's result")
			.and_then(|_| serialized_result_val)
			.and_then(|serialized_result_val| deserialize_result(&serialized_result_val))
	}
}

fn with_guest_externals<FE, R, F>(
	supervisor_externals: &mut FE,
	sandbox_instance: &SandboxInstance<FE::SupervisorFuncRef>,
	state: u32,
	f: F,
) -> R
where
	FE: SandboxCapabilities,
	F: FnOnce(&mut GuestExternals<FE>) -> R,
{
	let mut guest_externals = GuestExternals {
		supervisor_externals,
		sandbox_instance,
		state,
	};
	f(&mut guest_externals)
}

enum BackendInstance {
	Wasmi(wasmi::ModuleRef),
	Wasmtime(wasmtime::Instance),
	Wasmer(wasmer::Instance),
}

/// Sandboxed instance of a wasm module.
///
/// It's primary purpose is to [`invoke`] exported functions on it.
///
/// All imports of this instance are specified at the creation time and
/// imports are implemented by the supervisor.
///
/// Hence, in order to invoke an exported function on a sandboxed module instance,
/// it's required to provide supervisor externals: it will be used to execute
/// code in the supervisor context.
///
/// This is generic over a supervisor function reference type.
///
/// [`invoke`]: #method.invoke
pub struct SandboxInstance<FR> {
	backend_instance: BackendInstance,
	dispatch_thunk: FR,
	guest_to_supervisor_mapping: GuestToSupervisorFunctionMapping,
}

impl<FR> SandboxInstance<FR> {
	/// Invoke an exported function by a name.
	///
	/// `supervisor_externals` is required to execute the implementations
	/// of the syscalls that published to a sandboxed module instance.
	///
	/// The `state` parameter can be used to provide custom data for
	/// these syscall implementations.
	pub fn invoke<'a, FE, SCH>(
		&self,

		// function to call that is exported from the module
		export_name: &str,

		// arguments passed to the function
		args: &[RuntimeValue],

		// supervisor environment provided to the module
		// supervisor_externals: &mut FE,

		// arbitraty context data of the call
		state: u32,
	) -> std::result::Result<Option<wasmi::RuntimeValue>, wasmi::Error>
	where
		FE: SandboxCapabilities<SupervisorFuncRef = FR> + 'a,
		SCH: SandboxCapabiliesHolder<SupervisorFuncRef = FR, SC = FE>
	{
		SCH::with_sandbox_capabilities( |supervisor_externals| {
			with_guest_externals(
				supervisor_externals,
				self,
				state,
				|guest_externals| {
					match &self.backend_instance {
						BackendInstance::Wasmi(wasmi_instance) => {
							let wasmi_result = wasmi_instance
								.invoke_export(export_name, args, guest_externals)?;
							
							Ok(wasmi_result)
						}

						BackendInstance::Wasmtime(wasmtime_instance) => {
							let wasmtime_function = wasmtime_instance
								.get_func(export_name)
								.ok_or(wasmi::Error::Function("wasmtime function failed".to_string()))?;

							let args: Vec<Val> = args
								.iter()
								.map(|v| match *v {
									RuntimeValue::I32(val) => Val::I32(val),
									RuntimeValue::I64(val) => Val::I64(val),
									RuntimeValue::F32(val) => Val::F32(val.into()),
									RuntimeValue::F64(val) => Val::F64(val.into()),
								})
								.collect();

							let wasmtime_result = wasmtime_function
								.call(&args)
								.map_err(|e| wasmi::Error::Function(e.to_string()))?;

							assert!(wasmtime_result.len() < 2, "multiple return types are not supported yet");

							let wasmtime_result = if let Some(wasmtime_value) = wasmtime_result.first() {
								let wasmtime_value = match *wasmtime_value {
									Val::I32(val) => RuntimeValue::I32(val),
									Val::I64(val) => RuntimeValue::I64(val),
									Val::F32(val) => RuntimeValue::F32(val.into()),
									Val::F64(val) => RuntimeValue::F64(val.into()),
									_ => unreachable!(),
								};

								Some(wasmtime_value)
							} else {
								None
							};

							Ok(wasmtime_result)
						}

						BackendInstance::Wasmer(wasmer_instance) => {
							let function = wasmer_instance
								.exports
								.get_function(export_name)
								.map_err(|error| {
									println!("{:?}", error);
									wasmi::Error::Function("wasmer function failed".to_string())
								})?;

							let args: Vec<wasmer::Val> = args
								.iter()
								.map(|v| match *v {
									RuntimeValue::I32(val) => wasmer::Val::I32(val),
									RuntimeValue::I64(val) => wasmer::Val::I64(val),
									RuntimeValue::F32(val) => wasmer::Val::F32(val.into()),
									RuntimeValue::F64(val) => wasmer::Val::F64(val.into()),
								})
								.collect();

							let wasmer_result = function
								.call(&args)
								.map_err(|e| wasmi::Error::Function(e.to_string()))?;

							assert!(wasmer_result.len() < 2, "multiple return types are not supported yet");

							let wasmer_result = if let Some(wasmer_value) = wasmer_result.first() {
								let wasmer_value = match *wasmer_value {
									wasmer::Val::I32(val) => RuntimeValue::I32(val),
									wasmer::Val::I64(val) => RuntimeValue::I64(val),
									wasmer::Val::F32(val) => RuntimeValue::F32(val.into()),
									wasmer::Val::F64(val) => RuntimeValue::F64(val.into()),
									_ => unreachable!(),
								};

								Some(wasmer_value)
							} else {
								None
							};

							Ok(wasmer_result)
						}
					}
				},
			)
		})
	}

	/// Get the value from a global with the given `name`.
	///
	/// Returns `Some(_)` if the global could be found.
	pub fn get_global_val(&self, name: &str) -> Option<sp_wasm_interface::Value> {
		match &self.backend_instance {
			BackendInstance::Wasmi(wasmi_instance) => {
				let wasmi_global = wasmi_instance
					.export_by_name(name)?
					.as_global()?
					.get();

				Some(wasmi_global.into())
			}

			BackendInstance::Wasmtime(wasmtime_instance) => {
				let wasmtime_global = wasmtime_instance.get_global(name)?.get();
				let wasmtime_value = match wasmtime_global {
					Val::I32(val) => Value::I32(val),
					Val::I64(val) => Value::I64(val),
					Val::F32(val) => Value::F32(val),
					Val::F64(val) => Value::F64(val),
					_ => None?,
				};

				Some(wasmtime_value)
			}

			BackendInstance::Wasmer(wasmer_instance) => {
				let global = wasmer_instance.exports.get_global(name).ok()?;
				let wasmtime_value = match global.get() {
					wasmer::Val::I32(val) => Value::I32(val),
					wasmer::Val::I64(val) => Value::I64(val),
					wasmer::Val::F32(val) => Value::F32(f32::to_bits(val)),
					wasmer::Val::F64(val) => Value::F64(f64::to_bits(val)),
					_ => None?,
				};

				Some(wasmtime_value)
			}
		}
	}
}

/// Error occurred during instantiation of a sandboxed module.
pub enum InstantiationError {
	/// Something wrong with the environment definition. It either can't
	/// be decoded, have a reference to a non-existent or torn down memory instance.
	EnvironmentDefinitionCorrupted,
	/// Provided module isn't recognized as a valid webassembly binary.
	ModuleDecoding,
	/// Module is a well-formed webassembly binary but could not be instantiated. This could
	/// happen because, e.g. the module imports entries not provided by the environment.
	Instantiation,
	/// Module is well-formed, instantiated and linked, but while executing the start function
	/// a trap was generated.
	StartTrapped,
}

fn decode_environment_definition(
	raw_env_def: &[u8],
	memories: &[Option<Memory>],
) -> std::result::Result<(Imports, GuestToSupervisorFunctionMapping), InstantiationError> {
	let env_def = sandbox_primitives::EnvironmentDefinition::decode(&mut &raw_env_def[..])
		.map_err(|_| InstantiationError::EnvironmentDefinitionCorrupted)?;

	let mut func_map = HashMap::new();
	let mut memories_map = HashMap::new();
	let mut guest_to_supervisor_mapping = GuestToSupervisorFunctionMapping::new();

	for entry in &env_def.entries {
		let module = entry.module_name.clone();
		let field = entry.field_name.clone();

		match entry.entity {
			sandbox_primitives::ExternEntity::Function(func_idx) => {
				let externals_idx =
					guest_to_supervisor_mapping.define(SupervisorFuncIndex(func_idx as usize));
				func_map.insert((module, field), externals_idx);
			}
			sandbox_primitives::ExternEntity::Memory(memory_idx) => {
				let memory_ref = memories
					.get(memory_idx as usize)
					.cloned()
					.ok_or_else(|| InstantiationError::EnvironmentDefinitionCorrupted)?
					.ok_or_else(|| InstantiationError::EnvironmentDefinitionCorrupted)?;
				memories_map.insert((module, field), memory_ref);
			}
		}
	}

	Ok((
		Imports {
			func_map,
			memories_map,
		},
		guest_to_supervisor_mapping,
	))
}

/// An environment in which the guest module is instantiated.
pub struct GuestEnvironment {
	/// Function and memory imports of the guest module
	imports: Imports,

	/// Supervisor functinons mapped to guest index space
	guest_to_supervisor_mapping: GuestToSupervisorFunctionMapping,
}

impl GuestEnvironment {
	/// Decodes an environment definition from the given raw bytes.
	///
	/// Returns `Err` if the definition cannot be decoded.
	pub fn decode<FR>(
		store: &Store<FR>,
		raw_env_def: &[u8],
	) -> std::result::Result<Self, InstantiationError> {
		let (imports, guest_to_supervisor_mapping) = decode_environment_definition(raw_env_def, &store.memories)?;
		Ok(Self {
			imports,
			guest_to_supervisor_mapping,
		})
	}
}

/// An unregistered sandboxed instance.
///
/// To finish off the instantiation the user must call `register`.
#[must_use]
pub struct UnregisteredInstance<FR> {
	sandbox_instance: Rc<SandboxInstance<FR>>,
}

impl<FR> UnregisteredInstance<FR> {
	/// Finalizes instantiation of this module.
	pub fn register(self, store: &mut Store<FR>) -> u32 {
		// At last, register the instance.
		let instance_idx = store.register_sandbox_instance(self.sandbox_instance);
		instance_idx
	}
}

/// Helper type to provide sandbox capabilities to the inner context
pub trait SandboxCapabiliesHolder {
	/// Supervisor function reference
	type SupervisorFuncRef;

	/// Capabilities trait
	type SC: SandboxCapabilities<SupervisorFuncRef = Self::SupervisorFuncRef>;

	/// Wrapper that provides sandbox capabilities in a limited context
	fn with_sandbox_capabilities<R, F: FnOnce(&mut Self::SC) -> R>(f: F) -> R;
}

/// Sandbox backend to use
pub enum SandboxBackend {
	/// Wasm interpreter
	Wasmi,

	/// Wasmtime environment
	Wasmtime,

	/// Wasmer environment
	Wasmer,
}

#[derive(Clone, Debug)]
pub enum Memory {
	Wasmi(MemoryRef),
	// Wasmtime(todo),
	Wasmer(wasmer::Memory),
}

impl Memory {
	pub fn as_wasmi(&self) -> Option<MemoryRef> {
		match self {
			Memory::Wasmi(memory) => Some(memory.clone()),
			_ => None,
		}
	}

	pub fn as_wasmer(&self) -> Option<wasmer::Memory> {
		match self {
			Memory::Wasmer(memory) => Some(memory.clone()),
			_ => None,
		}
	}
}

struct WasmerBackend {
	compiler: Singlepass,
	store: wasmer::Store,
}

enum BackendContext {
	Wasmi,
	Wasmtime,
	Wasmer(WasmerBackend),
}

impl BackendContext {
	pub fn new(backend: SandboxBackend) -> BackendContext {
		match backend {
			SandboxBackend::Wasmi => BackendContext::Wasmi,
			SandboxBackend::Wasmtime => todo!(),

			SandboxBackend::Wasmer => {
				let compiler = Singlepass::default();

				BackendContext::Wasmer(
					WasmerBackend {
						store: wasmer::Store::new(&wasmer::JIT::new(&compiler).engine()),
						compiler,
					}
				)
			}
		}
	}
}

/// This struct keeps track of all sandboxed components.
///
/// This is generic over a supervisor function reference type.
pub struct Store<FR> {
	// Memories and instances are `Some` until torn down.
	instances: Vec<Option<Rc<SandboxInstance<FR>>>>,
	memories: Vec<Option<Memory>>,
	backend_context: BackendContext,
}

impl<FR> Store<FR> {
	fn allocate_memory(memories: &mut Vec<Option<Memory>>, backend_context: &BackendContext, initial: u32, maximum: u32) -> Result<(u32, Memory)> {
		let maximum = match maximum {
			sandbox_primitives::MEM_UNLIMITED => None,
			specified_limit => Some(specified_limit),
		};

		let memory = match &backend_context {
			BackendContext::Wasmi => {
				println!("creating wasmi memory {}..{}", initial, maximum.map(|v| v.to_string()).unwrap_or("?".into()));

				Memory::Wasmi(MemoryInstance::alloc(
					Pages(initial as usize),
					maximum.map(|m| Pages(m as usize)),
				)?)
			}

			BackendContext::Wasmer(context) => {
				let ty = wasmer::MemoryType::new(initial, maximum, false);
				println!("creating wasmer memory {:?}", ty);

				// let bt = backtrace::Backtrace::new();
				// println!("{:?}", bt);

				Memory::Wasmer(
					wasmer::Memory::new(&context.store, ty)
						.map_err(|r| {
							println!("Error creating wasmer Memory: {}", r.to_string());
							Error::InvalidMemoryReference
						})?
				)
			}

			BackendContext::Wasmtime => todo!(),
		};

		let mem_idx = memories.len();
		memories.push(Some(memory.clone()));

		Ok((mem_idx as u32, memory))
	}
}

impl<FR> Store<FR> {
	/// Create a new empty sandbox store.
	pub fn new(backend: SandboxBackend) -> Self {
		Store {
			instances: Vec::new(),
			memories: Vec::new(),
			backend_context: BackendContext::new(backend),
		}
	}

	/// Create a new memory instance and return it's index.
	///
	/// # Errors
	///
	/// Returns `Err` if the memory couldn't be created.
	/// Typically happens if `initial` is more than `maximum`.
	pub fn new_memory(&mut self, initial: u32, maximum: u32) -> Result<u32> {
		let memories = &mut self.memories;
		let backend_context = &self.backend_context;
		dbg!(initial, maximum);
		dbg!(Self::allocate_memory(memories, backend_context, initial, maximum).map(|(index, _)| index))
	}

	/// Returns `SandboxInstance` by `instance_idx`.
	///
	/// # Errors
	///
	/// Returns `Err` If `instance_idx` isn't a valid index of an instance or
	/// instance is already torndown.
	pub fn instance(&self, instance_idx: u32) -> Result<Rc<SandboxInstance<FR>>> {
		self.instances
			.get(instance_idx as usize)
			.cloned()
			.ok_or_else(|| "Trying to access a non-existent instance")?
			.ok_or_else(|| "Trying to access a torndown instance".into())
	}

	/// Returns reference to a memory instance by `memory_idx`.
	///
	/// # Errors
	///
	/// Returns `Err` If `memory_idx` isn't a valid index of an memory or
	/// if memory has been torn down.
	pub fn memory(&self, memory_idx: u32) -> Result<Memory> {
		self.memories
			.get(memory_idx as usize)
			.cloned()
			.ok_or_else(|| "Trying to access a non-existent sandboxed memory")?
			.ok_or_else(|| "Trying to access a torndown sandboxed memory".into())
	}

	/// Tear down the memory at the specified index.
	///
	/// # Errors
	///
	/// Returns `Err` if `memory_idx` isn't a valid index of an memory or
	/// if it has been torn down.
	pub fn memory_teardown(&mut self, memory_idx: u32) -> Result<()> {
		match self.memories.get_mut(memory_idx as usize) {
			None => Err("Trying to teardown a non-existent sandboxed memory".into()),
			Some(None) => Err("Double teardown of a sandboxed memory".into()),
			Some(memory) => {
				*memory = None;
				Ok(())
			}
		}
	}

	/// Tear down the instance at the specified index.
	///
	/// # Errors
	///
	/// Returns `Err` if `instance_idx` isn't a valid index of an instance or
	/// if it has been torn down.
	pub fn instance_teardown(&mut self, instance_idx: u32) -> Result<()> {
		match self.instances.get_mut(instance_idx as usize) {
			None => Err("Trying to teardown a non-existent instance".into()),
			Some(None) => Err("Double teardown of an instance".into()),
			Some(instance) => {
				*instance = None;
				Ok(())
			}
		}
	}

	fn register_sandbox_instance(&mut self, sandbox_instance: Rc<SandboxInstance<FR>>) -> u32 {
		let instance_idx = self.instances.len();
		self.instances.push(Some(sandbox_instance));
		instance_idx as u32
	}

	/// Instantiate a guest module and return it's index in the store.
	///
	/// The guest module's code is specified in `wasm`. Environment that will be available to
	/// guest module is specified in `guest_env`, `dispatch_thunk` is used as function that
	/// handle calls from guests. `state` is an opaque pointer to caller's arbitrary context
	/// normally created by `sp_sandbox::Instance` primitive.
	///
	/// Returns uninitialized sandboxed module instance or an instantiation error.
	pub fn instantiate<'a, FE, SCH>(
		&mut self,
		dispatch_thunk: FR,
		wasm: &[u8],
		guest_env: GuestEnvironment,
		state: u32,
	) -> std::result::Result<UnregisteredInstance<FR>, InstantiationError>
	where
		FR: Clone + 'static,
		FE: SandboxCapabilities<SupervisorFuncRef = FR> + 'a,
		SCH: SandboxCapabiliesHolder<SupervisorFuncRef = FR, SC = FE>,
	{
		let memories = &mut self.memories;
		let backend_context = &self.backend_context;

		let sandbox_instance = match backend_context {
			BackendContext::Wasmi => {
				let wasmi_module = Module::from_buffer(wasm).map_err(|_| InstantiationError::ModuleDecoding)?;
				let wasmi_instance = ModuleInstance::new(&wasmi_module, &guest_env.imports)
					.map_err(|_| InstantiationError::Instantiation)?;

				let sandbox_instance = Rc::new(SandboxInstance {
					// In general, it's not a very good idea to use `.not_started_instance()` for anything
					// but for extracting memory and tables. But in this particular case, we are extracting
					// for the purpose of running `start` function which should be ok.
					backend_instance: BackendInstance::Wasmi(wasmi_instance.not_started_instance().clone()),
					dispatch_thunk,
					guest_to_supervisor_mapping: guest_env.guest_to_supervisor_mapping,
				});

				SCH::with_sandbox_capabilities( |supervisor_externals| {
					with_guest_externals(
						supervisor_externals,
						&sandbox_instance,
						state,
						|guest_externals| {
							wasmi_instance
								.run_start(guest_externals)
								.map_err(|_| InstantiationError::StartTrapped)

							// Note: no need to run start on wasmtime instance, since it's done automatically
						},
					)
				})?;

				sandbox_instance
			}

			BackendContext::Wasmtime => {
				let mut config = wasmtime::Config::new();
				config.cranelift_opt_level(wasmtime::OptLevel::None);
				config.strategy(wasmtime::Strategy::Cranelift).map_err(|_| InstantiationError::ModuleDecoding)?;

				let wasmtime_engine = wasmtime::Engine::new(&config);
				let wasmtime_store = wasmtime::Store::new(&wasmtime_engine);
				let wasmtime_module = wasmtime::Module::new(&wasmtime_engine, wasm).map_err(|_| InstantiationError::ModuleDecoding)?;
				
				let module_imports: Vec<_> = wasmtime_module
					.imports()
					.filter_map(|import| {
						if let wasmtime::ExternType::Func(func_ty) = import.ty() {
							let guest_func_index = if let Some(index) = guest_env.imports.func_by_name(import.module(), import.name()) {
								index
							} else {
								// Missing import
								return None;
							};

							let supervisor_func_index = guest_env.guest_to_supervisor_mapping
								.func_by_guest_index(guest_func_index).expect("missing guest to host mapping");

							let dispatch_thunk = dispatch_thunk.clone();
							Some(wasmtime::Extern::Func(wasmtime::Func::new(&wasmtime_store, func_ty,
								move |_caller, params, result| {
									SCH::with_sandbox_capabilities(|supervisor_externals| {
										// Serialize arguments into a byte vector.
										let invoke_args_data = params
											.iter()
											.map(|val| match val {
												Val::I32(val) => Value::I32(*val),
												Val::I64(val) => Value::I64(*val),
												Val::F32(val) => Value::F32(*val),
												Val::F64(val) => Value::F64(*val),
												_ => unimplemented!()
											})
											.collect::<Vec<_>>()
											.encode();

										// Move serialized arguments inside the memory, invoke dispatch thunk and
										// then free allocated memory.
										let invoke_args_len = invoke_args_data.len() as WordSize;
										let invoke_args_ptr = supervisor_externals
											.allocate_memory(invoke_args_len)
											.map_err(|_| wasmtime::Trap::new("Can't allocate memory in supervisor for the arguments"))?;

										let deallocate = |fe: &mut FE, ptr, fail_msg| {
											fe
												.deallocate_memory(ptr)
												.map_err(|_| wasmtime::Trap::new(fail_msg))
										};

										if supervisor_externals
											.write_memory(invoke_args_ptr, &invoke_args_data)
											.is_err()
										{
											deallocate(supervisor_externals, invoke_args_ptr, "Failed dealloction after failed write of invoke arguments")?;
											return Err(wasmtime::Trap::new("Can't write invoke args into memory"));
										}

										let serialized_result = supervisor_externals.invoke(
											&dispatch_thunk,
											invoke_args_ptr,
											invoke_args_len,
											state,
											supervisor_func_index,
										)
											.map_err(|e| wasmtime::Trap::new(e.to_string()))?;

										// dispatch_thunk returns pointer to serialized arguments.
										// Unpack pointer and len of the serialized result data.
										let (serialized_result_val_ptr, serialized_result_val_len) = {
											// Cast to u64 to use zero-extension.
											let v = serialized_result as u64;
											let ptr = (v as u64 >> 32) as u32;
											let len = (v & 0xFFFFFFFF) as u32;
											(Pointer::new(ptr), len)
										};

										let serialized_result_val = supervisor_externals
											.read_memory(serialized_result_val_ptr, serialized_result_val_len)
											.map_err(|_| wasmtime::Trap::new("Can't read the serialized result from dispatch thunk"));

										let deserialized_result = deallocate(supervisor_externals, serialized_result_val_ptr, "Can't deallocate memory for dispatch thunk's result")
											.and_then(|_| serialized_result_val)
											.and_then(|serialized_result_val| {
												deserialize_result(&serialized_result_val)
													.map_err(|e| wasmtime::Trap::new(e.to_string()))
											})?;

										if let Some(value) = deserialized_result {
											result[0] = match value {
												RuntimeValue::I32(val) => Val::I32(val),
												RuntimeValue::I64(val) => Val::I64(val),
												RuntimeValue::F32(val) => Val::F32(val.into()),
												RuntimeValue::F64(val) => Val::F64(val.into()),
											}
										}

										Ok(())
									})
								}
							)))
						} else {
							None
						}
					})
					.collect();

				let wasmtime_instance = wasmtime::Instance::new(&wasmtime_store, &wasmtime_module, &module_imports).map_err(|error|
					if let Ok(_trap) = error.downcast::<wasmtime::Trap>() {
						InstantiationError::StartTrapped
					} else {
						InstantiationError::Instantiation
					}
				)?;

				Rc::new(SandboxInstance {
					backend_instance: BackendInstance::Wasmtime(wasmtime_instance),
					dispatch_thunk,
					guest_to_supervisor_mapping: guest_env.guest_to_supervisor_mapping,
				})
			}

			BackendContext::Wasmer(context) => {
				// let compiler = Singlepass::default();
				// let store = wasmer::Store::new(&wasmer::JIT::new(&compiler).engine());

				println!("Decoding module...");
				let module = wasmer::Module::new(&context.store, wasm).map_err(|error| {
					println!("{:?}", error);
					InstantiationError::ModuleDecoding
				})?;

				println!("Module name is {}", module.name().unwrap_or("(unknown)"));

				type Exports = HashMap<String, wasmer::Exports>;
				let mut exports_map = Exports::new();

				for import in module
					.imports()
					.into_iter()
				{
					match import.ty() {
						wasmer::ExternType::Global(global) => {
							println!("Importing global '{}' :: '{}' {}", import.module(), import.name(), global.to_string());
						}

						wasmer::ExternType::Table(table) => {
							println!("Importing table '{}' :: '{}' {}", import.module(), import.name(), table.to_string());
						}

						wasmer::ExternType::Memory(memory_type) => {
							println!("Importing memory '{}' :: '{}' {}", import.module(), import.name(), memory_type.to_string());
							let exports = exports_map.entry(import.module().to_string()).or_insert(wasmer::Exports::new());
							let memory = guest_env.imports.memory_by_name(import.module(), import.name()).ok_or(InstantiationError::ModuleDecoding)?;

							exports.insert(import.name(), wasmer::Extern::Memory(memory.as_wasmer().unwrap()));
						}

						wasmer::ExternType::Function(func_ty) => {
							println!("Importing function '{}' :: '{}' {}", import.module(), import.name(), func_ty.to_string());

							let guest_func_index = if let Some(index) = guest_env.imports.func_by_name(import.module(), import.name()) {
								index
							} else {
								// Missing import
								println!("Missing import '{}' :: '{}'", import.module(), import.name());
								continue;
							};

							let supervisor_func_index = guest_env.guest_to_supervisor_mapping
								.func_by_guest_index(guest_func_index).expect("missing guest to host mapping");

							let dispatch_thunk = dispatch_thunk.clone();
							let function = wasmer::Function::new(&context.store, func_ty, move |params| {
								SCH::with_sandbox_capabilities(|supervisor_externals| {
									// Serialize arguments into a byte vector.
									let invoke_args_data = params
										.iter()
										.map(|val| match val {
											wasmer::Val::I32(val) => Value::I32(*val),
											wasmer::Val::I64(val) => Value::I64(*val),
											wasmer::Val::F32(val) => Value::F32(f32::to_bits(*val)),
											wasmer::Val::F64(val) => Value::F64(f64::to_bits(*val)),
											_ => unimplemented!()
										})
										.collect::<Vec<_>>()
										.encode();

									// Move serialized arguments inside the memory, invoke dispatch thunk and
									// then free allocated memory.
									let invoke_args_len = invoke_args_data.len() as WordSize;
									let invoke_args_ptr = supervisor_externals
										.allocate_memory(invoke_args_len)
										.map_err(|_| wasmer::RuntimeError::new("Can't allocate memory in supervisor for the arguments"))?;

									let deallocate = |fe: &mut FE, ptr, fail_msg| {
										fe
											.deallocate_memory(ptr)
											.map_err(|_| wasmer::RuntimeError::new(fail_msg))
									};

									if supervisor_externals
										.write_memory(invoke_args_ptr, &invoke_args_data)
										.is_err()
									{
										deallocate(supervisor_externals, invoke_args_ptr, "Failed dealloction after failed write of invoke arguments")?;
										return Err(wasmer::RuntimeError::new("Can't write invoke args into memory"));
									}

									let serialized_result = supervisor_externals.invoke(
										&dispatch_thunk,
										invoke_args_ptr,
										invoke_args_len,
										state,
										supervisor_func_index,
									)
										.map_err(|e| wasmer::RuntimeError::new(e.to_string()))?;

									// dispatch_thunk returns pointer to serialized arguments.
									// Unpack pointer and len of the serialized result data.
									let (serialized_result_val_ptr, serialized_result_val_len) = {
										// Cast to u64 to use zero-extension.
										let v = serialized_result as u64;
										let ptr = (v as u64 >> 32) as u32;
										let len = (v & 0xFFFFFFFF) as u32;
										(Pointer::new(ptr), len)
									};

									let serialized_result_val = supervisor_externals
										.read_memory(serialized_result_val_ptr, serialized_result_val_len)
										.map_err(|_| wasmer::RuntimeError::new("Can't read the serialized result from dispatch thunk"));

									let deserialized_result = deallocate(supervisor_externals, serialized_result_val_ptr, "Can't deallocate memory for dispatch thunk's result")
										.and_then(|_| serialized_result_val)
										.and_then(|serialized_result_val| {
											deserialize_result(&serialized_result_val)
												.map_err(|e| wasmer::RuntimeError::new(e.to_string()))
										})?;

									if let Some(value) = deserialized_result {
										Ok(vec![
											match value {
												RuntimeValue::I32(val) => wasmer::Val::I32(val),
												RuntimeValue::I64(val) => wasmer::Val::I64(val),
												RuntimeValue::F32(val) => wasmer::Val::F32(val.into()),
												RuntimeValue::F64(val) => wasmer::Val::F64(val.into()),
											}
										])
									} else {
										Ok(vec![])
									}
								})
							});

							let exports = exports_map.entry(import.module().to_string()).or_insert(wasmer::Exports::new());
							exports.insert(import.name(), wasmer::Extern::Function(function));
						}
					}
				}

				let mut import_object = wasmer::ImportObject::new();

				for (module_name, exports) in exports_map.into_iter() {
					import_object.register(module_name, exports);
				}

				println!("Instantiating module...");
				let instance = wasmer::Instance::new(&module, &import_object)
					.map_err(|error| {
						println!("{:?}", error);

						match error {
							wasmer::InstantiationError::Link(_) => InstantiationError::Instantiation,
							wasmer::InstantiationError::Start(_) => InstantiationError::StartTrapped,
						}
					})?;

				println!("Creating SandboxInstance...");
				Rc::new(SandboxInstance {
					backend_instance: BackendInstance::Wasmer(instance),
					dispatch_thunk,
					guest_to_supervisor_mapping: guest_env.guest_to_supervisor_mapping,
				})
			}
		};

		Ok(UnregisteredInstance { sandbox_instance })
	}

}
