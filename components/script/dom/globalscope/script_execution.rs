/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::ffi::CStr;
use std::ptr::NonNull;
use std::rc::Rc;

use content_security_policy::sandboxing_directive::SandboxingFlagSet;
use js::context::JSContext;
use js::jsapi::{ExceptionStackBehavior, Heap, JSScript, SetScriptPrivate};
use js::jsval::{ObjectValue, PrivateValue, UndefinedValue};
use js::panic::maybe_resume_unwind;
use js::rust::wrappers2::{
    Compile1, JS_ClearPendingException, JS_ExecuteScript, JS_GetScriptPrivate,
    JS_IsExceptionPending, JS_SetPendingException,
};
use js::rust::{
    CompileOptionsWrapper, HandleValue, MutableHandleValue, transform_str_to_source_text,
};
use js::{rooted, rooted_vec};
use script_bindings::cformat;
use script_bindings::settings_stack::run_a_script;
use script_bindings::trace::RootedTraceableBox;
use servo_config::pref;
use servo_url::ServoUrl;

use crate::DomTypeHolder;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::{
    Error, ErrorInfo, ErrorResult, report_pending_exception, throw_dom_exception,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::globalscope::GlobalScope;
use crate::dom::window::Window;
use crate::modules::script_module::{
    ModuleScript, ModuleSource, ModuleTree, RethrowError, ScriptFetchOptions,
};
use crate::realms::enter_auto_realm;
use crate::unminify::unminify_js;

/// <https://html.spec.whatwg.org/multipage/#classic-script>
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct ClassicScript {
    /// On script parsing success this will be <https://html.spec.whatwg.org/multipage/#concept-script-record>
    /// On failure <https://html.spec.whatwg.org/multipage/#concept-script-error-to-rethrow>
    #[ignore_malloc_size_of = "mozjs"]
    pub record: Result<RootedTraceableBox<Heap<*mut JSScript>>, RethrowError>,
    /// <https://html.spec.whatwg.org/multipage/#concept-script-script-fetch-options>
    fetch_options: ScriptFetchOptions,
    /// <https://html.spec.whatwg.org/multipage/#concept-script-base-url>
    #[no_trace]
    url: ServoUrl,
    /// <https://html.spec.whatwg.org/multipage/#muted-errors>
    muted_errors: ErrorReporting,
}

#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
pub(crate) enum ErrorReporting {
    Muted,
    Unmuted,
}

impl From<bool> for ErrorReporting {
    fn from(boolean: bool) -> Self {
        if boolean {
            ErrorReporting::Muted
        } else {
            ErrorReporting::Unmuted
        }
    }
}

pub(crate) enum RethrowErrors {
    Yes,
    No,
}

impl GlobalScope {
    /// <https://html.spec.whatwg.org/multipage/#creating-a-classic-script>
    #[expect(clippy::too_many_arguments)]
    #[expect(unsafe_code)]
    pub(crate) fn create_a_classic_script(
        &self,
        cx: &mut JSContext,
        source: Cow<'_, str>,
        url: ServoUrl,
        fetch_options: ScriptFetchOptions,
        muted_errors: ErrorReporting,
        introduction_type: Option<&'static CStr>,
        line_number: u32,
        external: bool,
    ) -> ClassicScript {
        let mut source = if self.unminified_js_dir().is_some() {
            let mut script_source = ModuleSource {
                source,
                unminified_dir: self.unminified_js_dir(),
                external,
                url: url.clone(),
            };
            unminify_js(&mut script_source);
            transform_str_to_source_text(&script_source.source)
        } else {
            transform_str_to_source_text(&source)
        };

        // TODO Step 1. If mutedErrors is true, then set baseURL to about:blank.

        // TODO Step 2. If scripting is disabled for settings, then set source to the empty string.

        // TODO Step 4. Set script's settings object to settings.

        // TODO Step 9. Record classic script creation time given script and sourceURLForWindowScripts.

        let options = fill_compile_options(
            cx,
            url.as_str(),
            introduction_type,
            muted_errors,
            true, // noScriptRval
            line_number,
        );

        // Step 10. Let result be ParseScript(source, settings's realm, script).
        rooted!(&in(cx) let compiled_script = unsafe { Compile1(cx, options.ptr, &mut source) });

        // Step 11. If result is a list of errors, then:
        let record = if compiled_script.get().is_null() {
            // Step 11.1. Set script's parse error and its error to rethrow to result[0].
            // Step 11.2. Return script.
            Err(RethrowError::from_pending_exception(cx))
        } else {
            Ok(RootedTraceableBox::from_box(Heap::boxed(
                compiled_script.get(),
            )))
        };

        // Step 3. Let script be a new classic script that this algorithm will subsequently initialize.
        // Step 5. Set script's base URL to baseURL.
        // Step 6. Set script's fetch options to options.
        // Step 7. Set script's muted errors to mutedErrors.
        // Step 12. Set script's record to result.
        // Step 13. Return script.
        ClassicScript {
            record,
            url,
            fetch_options,
            muted_errors,
        }
    }

    /// Runs a `<script type="application/wasm">` module.
    ///
    /// Experimental and non-standard. Mirrors the shape of `run_a_classic_script`: bail if
    /// script cannot run, enter the realm, then compile → build imports → instantiate →
    /// call the start export. Any failure at any step surfaces as a pending JS exception
    /// reported through the ordinary `window.onerror` path, exactly as a JS script would.
    #[cfg(feature = "wasm_dom")]
    pub(crate) fn run_a_wasm_dom_script(
        &self,
        cx: &mut JSContext,
        script: crate::dom::html::htmlscriptelement::WasmScript,
    ) {
        use script_bindings::reflector::DomObject;

        use crate::dom::security::csp::CspReporting;
        use crate::wasm_dom::bridge::build_import_object;
        use crate::wasm_dom::instance::WasmDomInstance;
        use crate::wasm_dom::instantiate::{compile_module, instance_export, instantiate_module};

        if !self.can_run_script() {
            return;
        }

        // Belt and braces alongside SpiderMonkey's own CanCompileStrings hook, which fires
        // inside the Module constructor. Checking here too means a CSP-blocked module fails
        // before compilation rather than as a thrown CompileError.
        if !self.get_csp_list().is_wasm_evaluation_allowed(cx, self) {
            warn!(
                "wasm-dom: blocked by Content Security Policy: {}",
                script.url
            );
            // Returning here skips the compile whose CanCompileStrings hook would have
            // thrown visibly, so raise the equivalent ourselves: a silent CSP block is
            // indistinguishable from a broken module, which cost a debugging session once.
            let mut realm = enter_auto_realm(cx, self);
            let cx = &mut realm.current_realm();
            throw_dom_exception(
                cx,
                self,
                Error::Security(Some(format!(
                    "wasm-dom: refused to compile {}: blocked by Content Security Policy",
                    script.url
                ))),
            );
            report_pending_exception(cx);
            return;
        }

        let mut realm = enter_auto_realm(cx, self);
        let cx = &mut realm.current_realm();

        run_a_script::<DomTypeHolder, _, _>(cx, self, |cx| {
            rooted!(&in(cx) let global_object = self.reflector().get_jsobject().get());

            let Ok(module) = compile_module(cx, global_object.handle(), &script.bytes) else {
                report_pending_exception(cx);
                return;
            };
            rooted!(&in(cx) let module = module);

            // Register before building imports: the import object embeds the instance index.
            let instance = Rc::new(WasmDomInstance::new(pref!(dom_wasm_dom_max_handles)));
            let index = self.register_wasm_dom_instance(instance.clone());

            let Ok(imports) = build_import_object(cx, index) else {
                throw_dom_exception(
                    cx,
                    self,
                    Error::Type(c"wasm-dom: could not build the import object".to_owned()),
                );
                report_pending_exception(cx);
                return;
            };
            rooted!(&in(cx) let imports = imports);

            let Ok(wasm_instance) = instantiate_module(
                cx,
                global_object.handle(),
                module.handle(),
                imports.handle(),
            ) else {
                // A LinkError here usually means the module imported something not exposed.
                report_pending_exception(cx);
                return;
            };
            rooted!(&in(cx) let wasm_instance = wasm_instance);

            // Memory is required: every string argument travels through it.
            let Ok(memory) = instance_export(cx, wasm_instance.handle(), c"memory") else {
                // A thrown TypeError reaches window.onerror and the console; a warn! reaches
                // nobody. This exact silence pattern has already cost a debugging session
                // twice on this branch.
                throw_dom_exception(
                    cx,
                    self,
                    Error::Type(c"wasm-dom: module must export its memory as `memory`".to_owned()),
                );
                report_pending_exception(cx);
                return;
            };
            instance.set_memory(memory);

            // Optional: a module that registers no listeners never needs one.
            if let Ok(dispatcher) =
                instance_export(cx, wasm_instance.handle(), c"_servo_dom_dispatch")
            {
                instance.set_dispatcher(dispatcher);
            }

            // `_servo_dom_start`, else `_start` for WASI-style toolchains.
            let start = instance_export(cx, wasm_instance.handle(), c"_servo_dom_start")
                .or_else(|_| instance_export(cx, wasm_instance.handle(), c"_start"));
            let Ok(start) = start else {
                throw_dom_exception(
                    cx,
                    self,
                    Error::Type(
                        c"wasm-dom: module exports neither _servo_dom_start nor _start".to_owned(),
                    ),
                );
                report_pending_exception(cx);
                return;
            };

            rooted!(&in(cx) let start_value = ObjectValue(start));
            // Separate roots: one value cannot serve as both `this` and the return slot.
            rooted!(&in(cx) let this_value = UndefinedValue());
            rooted!(&in(cx) let mut return_value = UndefinedValue());
            rooted_vec!(let mut no_arguments);
            let args = js::jsapi::HandleValueArray::from(&no_arguments);
            // SAFETY: calling a rooted exported function with no arguments.
            #[expect(unsafe_code)]
            let ok = unsafe {
                js::rust::wrappers2::Call(
                    cx,
                    this_value.handle().into(),
                    start_value.handle(),
                    &args,
                    return_value.handle_mut(),
                )
            };
            if !ok {
                // A trap arrives here as a pending RuntimeError.
                report_pending_exception(cx);
            }
        });
    }

    /// <https://html.spec.whatwg.org/multipage/#run-a-classic-script>
    #[expect(unsafe_code)]
    pub(crate) fn run_a_classic_script(
        &self,
        cx: &mut JSContext,
        script: ClassicScript,
        rethrow_errors: RethrowErrors,
    ) -> ErrorResult {
        // TODO Step 1. Let settings be the settings object of script.

        // Step 2. Check if we can run script with settings. If this returns "do not run", then return NormalCompletion(empty).
        if !self.can_run_script() {
            return Ok(());
        }

        // TODO Step 3. Record classic script execution start time given script.

        let mut realm = enter_auto_realm(cx, self);
        let cx = &mut realm.current_realm();

        // Step 4. Prepare to run script given settings.
        // Once dropped this will run "Step 9. Clean up after running script" steps
        run_a_script::<DomTypeHolder, _, _>(cx, self, |cx| {
            // Step 5. Let evaluationStatus be null.
            let mut result = false;

            match script.record {
                // Step 6. If script's error to rethrow is not null, then set evaluationStatus to ThrowCompletion(script's error to rethrow).
                Err(error_to_rethrow) => unsafe {
                    JS_SetPendingException(
                        cx,
                        error_to_rethrow.handle(),
                        ExceptionStackBehavior::Capture,
                    )
                },
                // Step 7. Otherwise, set evaluationStatus to ScriptEvaluation(script's record).
                Ok(compiled_script) => {
                    rooted!(&in(cx) let mut rval = UndefinedValue());
                    let script_ptr = NonNull::new(compiled_script.get())
                        .expect("Compiled script must not be null");
                    result = evaluate_script(
                        cx,
                        script_ptr,
                        script.url,
                        script.fetch_options,
                        rval.handle_mut(),
                    );
                },
            }

            // Step 8. If evaluationStatus is an abrupt completion, then:
            if unsafe { JS_IsExceptionPending(cx) } {
                warn!("Error evaluating script");

                match rethrow_errors {
                    RethrowErrors::Yes => {
                        match script.muted_errors {
                            // Step 8.1. If rethrow errors is true and script's muted errors is false, then:
                            // Rethrow evaluationStatus.[[Value]].
                            ErrorReporting::Unmuted => return Err(Error::JSFailed),
                            // Step 8.2. If rethrow errors is true and script's muted errors is true, then:
                            ErrorReporting::Muted => {
                                unsafe { JS_ClearPendingException(cx) };
                                // Throw a "NetworkError" DOMException.
                                return Err(Error::Network(None));
                            },
                        }
                    },
                    // Step 8.3. Otherwise, rethrow errors is false. Perform the following steps:
                    RethrowErrors::No => {
                        // Report an exception given by evaluationStatus.[[Value]] for script's
                        // settings object's global object.
                        match script.muted_errors {
                            ErrorReporting::Unmuted => {
                                report_pending_exception(cx);
                            },
                            ErrorReporting::Muted => {
                                unsafe { JS_ClearPendingException(cx) };
                                self.report_an_error(
                                    cx,
                                    ErrorInfo {
                                        message: String::from("Script error."),
                                        ..Default::default()
                                    },
                                    HandleValue::null(),
                                );
                            },
                        }
                        return Err(Error::JSFailed);
                    },
                }
            }

            maybe_resume_unwind();

            // Step 10. If evaluationStatus is a normal completion, then return evaluationStatus.
            if result {
                return Ok(());
            }

            // Step 11. If we've reached this point, evaluationStatus was left as null because the script
            // was aborted prematurely during evaluation. Return ThrowCompletion(a new QuotaExceededError).
            Err(Error::QuotaExceeded {
                quota: None,
                requested: None,
            })
        })
    }

    /// <https://html.spec.whatwg.org/multipage/#run-a-module-script>
    pub(crate) fn run_a_module_script(
        &self,
        cx: &mut JSContext,
        module_tree: Rc<ModuleTree>,
        _rethrow_errors: bool,
    ) {
        // Step 1. Let settings be the settings object of script.
        // NOTE(pylbrecht): "settings" is `self` here.

        // Step 2. Check if we can run script with settings. If this returns "do not run", then
        // return a promise resolved with undefined.
        if !self.can_run_script() {
            return;
        }

        // Step 3. Record module script execution start time given script.
        // TODO

        // Step 4. Prepare to run script given settings.
        run_a_script::<DomTypeHolder, _, _>(cx, self, |cx| {
            // Step 6. If script's error to rethrow is not null, then set evaluationPromise to a
            // promise rejected with script's error to rethrow.
            {
                let module_error = module_tree.get_rethrow_error().borrow();
                if module_error.is_some() {
                    module_tree.report_error(cx, self);
                    return;
                }
            }

            // Step 7.1. Otherwise: Let record be script's record.
            let record = module_tree.get_record().map(|record| record.handle());

            if let Some(record) = record {
                // Step 7.2. Set evaluationPromise to record.Evaluate().
                rooted!(&in(cx) let mut rval = UndefinedValue());
                let evaluated = module_tree.execute_module(cx, self, record, rval.handle_mut());

                // Step 8. If preventErrorReporting is false, then upon rejection of evaluationPromise
                // with reason, report an exception given by reason for script's settings object's
                // global object.
                if let Err(exception) = evaluated {
                    module_tree.set_rethrow_error(exception);
                    module_tree.report_error(cx, self);
                }
            }
        });
    }

    /// <https://html.spec.whatwg.org/multipage/#check-if-we-can-run-script>
    pub(crate) fn can_run_script(&self) -> bool {
        // Step 1 If the global object specified by settings is a Window object
        // whose Document object is not fully active, then return "do not run".
        //
        // Step 2 If scripting is disabled for settings, then return "do not run".
        //
        // An user agent can also disable scripting
        //
        // Either settings's global object is not a Window object,
        // or settings's global object's associated Document's active sandboxing flag set
        // does not have its sandboxed scripts browsing context flag set.
        if let Some(window) = self.downcast::<Window>() {
            let doc = window.Document();
            doc.is_fully_active() &&
                !doc.has_active_sandboxing_flag(
                    SandboxingFlagSet::SANDBOXED_SCRIPTS_BROWSING_CONTEXT_FLAG,
                )
        } else {
            true
        }
    }
}

pub(crate) fn fill_compile_options(
    cx: &mut JSContext,
    filename: &str,
    introduction_type: Option<&'static CStr>,
    muted_errors: ErrorReporting,
    no_script_rval: bool,
    line_number: u32,
) -> CompileOptionsWrapper {
    let muted_errors = match muted_errors {
        ErrorReporting::Muted => true,
        ErrorReporting::Unmuted => false,
    };

    // TODO: pass filename as CString to avoid allocation
    // See https://github.com/servo/servo/issues/42126
    let mut options = CompileOptionsWrapper::new(cx, cformat!("{filename}"), line_number);
    if let Some(introduction_type) = introduction_type {
        options.set_introduction_type(introduction_type);
    }

    // https://searchfox.org/firefox-main/rev/46fa95cd7f10222996ec267947ab94c5107b1475/js/public/CompileOptions.h#284
    options.set_muted_errors(muted_errors);

    // https://searchfox.org/firefox-main/rev/46fa95cd7f10222996ec267947ab94c5107b1475/js/public/CompileOptions.h#518
    options.set_is_run_once(true);
    options.set_no_script_rval(no_script_rval);

    options
}

/// <https://tc39.es/ecma262/#sec-runtime-semantics-scriptevaluation>
#[expect(unsafe_code)]
pub(crate) fn evaluate_script(
    cx: &mut JSContext,
    compiled_script: NonNull<JSScript>,
    url: ServoUrl,
    fetch_options: ScriptFetchOptions,
    rval: MutableHandleValue,
) -> bool {
    rooted!(&in(cx) let record = compiled_script.as_ptr());
    rooted!(&in(cx) let mut script_private = UndefinedValue());

    unsafe { JS_GetScriptPrivate(*record, script_private.handle_mut()) };

    // When `ScriptPrivate` for the compiled script is undefined,
    // we need to set it so that it can be used in dynamic import context.
    if script_private.is_undefined() {
        debug!("Set script private for {}", url);
        let module_script_data = Rc::new(ModuleScript::new(
            url,
            fetch_options,
            // We can't initialize an module owner here because
            // the executing context of script might be different
            // from the dynamic import script's executing context.
            None,
        ));

        unsafe {
            SetScriptPrivate(
                *record,
                &PrivateValue(Rc::into_raw(module_script_data) as *const _),
            );
        }
    }

    unsafe { JS_ExecuteScript(cx, record.handle(), rval) }
}
