// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(clippy::await_holding_refcell_ref)]
use crate::input_handler::{InputHandlerStatus, UnhandledInputHandler};
use crate::utils::{Position, Size};
use crate::{input_device, metrics, touch_binding};
use anyhow::{Context, Error, Result};
use async_trait::async_trait;
use async_utils::hanging_get::client::HangingGetStream;
use fidl::endpoints::create_proxy;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_inspect::health::Reporter;
use futures::stream::StreamExt;
use metrics_registry::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use {
    fidl_fuchsia_ui_pointerinjector as pointerinjector,
    fidl_fuchsia_ui_pointerinjector_configuration as pointerinjector_config,
};

/// An input handler that parses touch events and forwards them to Scenic through the
/// fidl_fuchsia_pointerinjector protocols.
pub struct TouchInjectorHandler {
    /// The mutable fields of this handler.
    mutable_state: RefCell<MutableState>,

    /// The scope and coordinate system of injection.
    /// See fidl_fuchsia_pointerinjector::Context for more details.
    context_view_ref: fidl_fuchsia_ui_views::ViewRef,

    /// The region where dispatch is attempted for injected events.
    /// See fidl_fuchsia_pointerinjector::Target for more details.
    target_view_ref: fidl_fuchsia_ui_views::ViewRef,

    /// The size of the display associated with the touch device, used to convert
    /// coordinates from the touch input report to device coordinates (which is what
    /// Scenic expects).
    display_size: Size,

    /// The FIDL proxy to register new injectors.
    injector_registry_proxy: pointerinjector::RegistryProxy,

    /// The FIDL proxy used to get configuration details for pointer injection.
    configuration_proxy: pointerinjector_config::SetupProxy,

    /// The inventory of this handler's Inspect status.
    pub inspect_status: InputHandlerStatus,

    /// The metrics logger.
    metrics_logger: metrics::MetricsLogger,
}

#[derive(Debug)]
struct MutableState {
    /// A rectangular region that directs injected events into a target.
    /// See fidl_fuchsia_pointerinjector::Viewport for more details.
    viewport: Option<pointerinjector::Viewport>,

    /// The injectors registered with Scenic, indexed by their device ids.
    injectors: HashMap<u32, pointerinjector::DeviceProxy>,
}

#[async_trait(?Send)]
impl UnhandledInputHandler for TouchInjectorHandler {
    async fn handle_unhandled_input_event(
        self: Rc<Self>,
        unhandled_input_event: input_device::UnhandledInputEvent,
    ) -> Vec<input_device::InputEvent> {
        fuchsia_trace::duration!(c"input", c"presentation_on_event");
        match unhandled_input_event {
            input_device::UnhandledInputEvent {
                device_event: input_device::InputDeviceEvent::TouchScreen(ref touch_event),
                device_descriptor:
                    input_device::InputDeviceDescriptor::TouchScreen(ref touch_device_descriptor),
                event_time,
                trace_id,
            } => {
                self.inspect_status.count_received_event(input_device::InputEvent::from(
                    unhandled_input_event.clone(),
                ));
                fuchsia_trace::duration!(c"input", c"touch_injector_handler");
                if let Some(trace_id) = trace_id {
                    fuchsia_trace::flow_end!(c"input", c"event_in_input_pipeline", trace_id.into());
                }
                // Create a new injector if this is the first time seeing device_id.
                if let Err(e) = self.ensure_injector_registered(&touch_device_descriptor).await {
                    self.metrics_logger.log_error(
                        InputPipelineErrorMetricDimensionEvent::TouchInjectorEnsureInjectorRegisteredFailed,
                        std::format!("ensure_injector_registered failed: {}", e));
                }

                // Handle the event.
                if let Err(e) = self
                    .send_event_to_scenic(&touch_event, &touch_device_descriptor, event_time)
                    .await
                {
                    self.metrics_logger.log_error(
                        InputPipelineErrorMetricDimensionEvent::TouchInjectorSendEventToScenicFailed,
                        std::format!("send_event_to_scenic failed: {}", e));
                }

                // Consume the input event.
                self.inspect_status.count_handled_event();
                vec![input_device::InputEvent::from(unhandled_input_event).into_handled()]
            }
            _ => vec![input_device::InputEvent::from(unhandled_input_event)],
        }
    }

    fn set_handler_healthy(self: std::rc::Rc<Self>) {
        self.inspect_status.health_node.borrow_mut().set_ok();
    }

    fn set_handler_unhealthy(self: std::rc::Rc<Self>, msg: &str) {
        self.inspect_status.health_node.borrow_mut().set_unhealthy(msg);
    }
}

impl TouchInjectorHandler {
    /// Creates a new touch handler that holds touch pointer injectors.
    /// The caller is expected to spawn a task to continually watch for updates to the viewport.
    /// Example:
    /// let handler = TouchInjectorHandler::new(display_size).await?;
    /// fasync::Task::local(handler.clone().watch_viewport()).detach();
    ///
    /// # Parameters
    /// - `display_size`: The size of the associated touch display.
    ///
    /// # Errors
    /// If unable to connect to pointerinjector protocols.
    pub async fn new(
        display_size: Size,
        input_handlers_node: &fuchsia_inspect::Node,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Rc<Self>, Error> {
        let configuration_proxy = connect_to_protocol::<pointerinjector_config::SetupMarker>()?;
        let injector_registry_proxy = connect_to_protocol::<pointerinjector::RegistryMarker>()?;

        Self::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            display_size,
            input_handlers_node,
            metrics_logger,
        )
        .await
    }

    /// Creates a new touch handler that holds touch pointer injectors.
    /// The caller is expected to spawn a task to continually watch for updates to the viewport.
    /// Example:
    /// let handler = TouchInjectorHandler::new_with_config_proxy(config_proxy, display_size).await?;
    /// fasync::Task::local(handler.clone().watch_viewport()).detach();
    ///
    /// # Parameters
    /// - `configuration_proxy`: A proxy used to get configuration details for pointer
    ///    injection.
    /// - `display_size`: The size of the associated touch display.
    ///
    /// # Errors
    /// If unable to get injection view refs from `configuration_proxy`.
    /// If unable to connect to pointerinjector Registry protocol.
    pub async fn new_with_config_proxy(
        configuration_proxy: pointerinjector_config::SetupProxy,
        display_size: Size,
        input_handlers_node: &fuchsia_inspect::Node,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Rc<Self>, Error> {
        let injector_registry_proxy = connect_to_protocol::<pointerinjector::RegistryMarker>()?;
        Self::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            display_size,
            input_handlers_node,
            metrics_logger,
        )
        .await
    }

    /// Creates a new touch handler that holds touch pointer injectors.
    /// The caller is expected to spawn a task to continually watch for updates to the viewport.
    /// Example:
    /// let handler = TouchInjectorHandler::new_handler(None, None, display_size).await?;
    /// fasync::Task::local(handler.clone().watch_viewport()).detach();
    ///
    /// # Parameters
    /// - `configuration_proxy`: A proxy used to get configuration details for pointer
    ///    injection.
    /// - `injector_registry_proxy`: A proxy used to register new pointer injectors.  If
    ///    none is provided, connect to protocol routed to this component.
    /// - `display_size`: The size of the associated touch display.
    ///
    /// # Errors
    /// If unable to get injection view refs from `configuration_proxy`.
    async fn new_handler(
        configuration_proxy: pointerinjector_config::SetupProxy,
        injector_registry_proxy: pointerinjector::RegistryProxy,
        display_size: Size,
        input_handlers_node: &fuchsia_inspect::Node,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Rc<Self>, Error> {
        // Get the context and target views to inject into.
        let (context_view_ref, target_view_ref) = configuration_proxy.get_view_refs().await?;

        let inspect_status = InputHandlerStatus::new(
            input_handlers_node,
            "touch_injector_handler",
            /* generates_events */ false,
        );
        let handler = Rc::new(Self {
            mutable_state: RefCell::new(MutableState { viewport: None, injectors: HashMap::new() }),
            context_view_ref,
            target_view_ref,
            display_size,
            injector_registry_proxy,
            configuration_proxy,
            inspect_status,
            metrics_logger,
        });

        Ok(handler)
    }

    /// Adds a new pointer injector and tracks it in `self.injectors` if one doesn't exist at
    /// `touch_descriptor.device_id`.
    ///
    /// # Parameters
    /// - `touch_descriptor`: The descriptor of the new touch device.
    async fn ensure_injector_registered(
        self: &Rc<Self>,
        touch_descriptor: &touch_binding::TouchScreenDeviceDescriptor,
    ) -> Result<(), anyhow::Error> {
        if self.mutable_state.borrow().injectors.contains_key(&touch_descriptor.device_id) {
            return Ok(());
        }

        // Create a new injector.
        let (device_proxy, device_server) = create_proxy::<pointerinjector::DeviceMarker>();
        let context = fuchsia_scenic::duplicate_view_ref(&self.context_view_ref)
            .context("Failed to duplicate context view ref.")?;
        let target = fuchsia_scenic::duplicate_view_ref(&self.target_view_ref)
            .context("Failed to duplicate target view ref.")?;
        let viewport = self.mutable_state.borrow().viewport.clone();
        if viewport.is_none() {
            // An injector without a viewport is not valid. The event will be dropped
            // since the handler will not have a registered injector to inject into.
            return Err(anyhow::format_err!(
                "Received a touch event without a viewport to inject into."
            ));
        }
        let config = pointerinjector::Config {
            device_id: Some(touch_descriptor.device_id),
            device_type: Some(pointerinjector::DeviceType::Touch),
            context: Some(pointerinjector::Context::View(context)),
            target: Some(pointerinjector::Target::View(target)),
            viewport,
            dispatch_policy: Some(pointerinjector::DispatchPolicy::TopHitAndAncestorsInTarget),
            scroll_v_range: None,
            scroll_h_range: None,
            buttons: None,
            ..Default::default()
        };

        // Keep track of the injector.
        self.mutable_state.borrow_mut().injectors.insert(touch_descriptor.device_id, device_proxy);

        // Register the new injector.
        self.injector_registry_proxy
            .register(config, device_server)
            .await
            .context("Failed to register injector.")?;
        log::info!("Registered injector with device id {:?}", touch_descriptor.device_id);

        Ok(())
    }

    /// Sends the given event to Scenic.
    ///
    /// # Parameters
    /// - `touch_event`: The touch event to send to Scenic.
    /// - `touch_descriptor`: The descriptor for the device that sent the touch event.
    /// - `event_time`: The time in nanoseconds when the event was first recorded.
    async fn send_event_to_scenic(
        &self,
        touch_event: &touch_binding::TouchScreenEvent,
        touch_descriptor: &touch_binding::TouchScreenDeviceDescriptor,
        event_time: zx::MonotonicInstant,
    ) -> Result<(), anyhow::Error> {
        // The order in which events are sent to clients.
        let ordered_phases = vec![
            pointerinjector::EventPhase::Add,
            pointerinjector::EventPhase::Change,
            pointerinjector::EventPhase::Remove,
        ];

        // Make the trace duration end on the call to injector.inject, not the call's return.
        // The duration should start before the flow_begin is minted in
        // create_pointer_sample_event, and it should not include the injector.inject() call's
        // return from await.
        fuchsia_trace::duration_begin!(c"input", c"touch-inject-into-scenic");

        let mut events: Vec<pointerinjector::Event> = vec![];
        for phase in ordered_phases {
            let contacts: Vec<touch_binding::TouchContact> = touch_event
                .injector_contacts
                .get(&phase)
                .map_or(vec![], |contacts| contacts.to_owned());
            let new_events = contacts.into_iter().map(|contact| {
                Self::create_pointer_sample_event(
                    phase,
                    &contact,
                    touch_descriptor,
                    &self.display_size,
                    event_time,
                )
            });
            events.extend(new_events);
        }

        let injector =
            self.mutable_state.borrow().injectors.get(&touch_descriptor.device_id).cloned();
        if let Some(injector) = injector {
            let fut = injector.inject(&events);
            // This trace duration ends before awaiting on the returned future.
            fuchsia_trace::duration_end!(c"input", c"touch-inject-into-scenic");
            let _ = fut.await;
            Ok(())
        } else {
            fuchsia_trace::duration_end!(c"input", c"touch-inject-into-scenic");
            Err(anyhow::format_err!(
                "No injector found for touch device {}.",
                touch_descriptor.device_id
            ))
        }
    }

    /// Creates a [`fidl_fuchsia_ui_pointerinjector::Event`] representing the given touch contact.
    ///
    /// # Parameters
    /// - `phase`: The phase of the touch contact.
    /// - `contact`: The touch contact to create the event for.
    /// - `touch_descriptor`: The device descriptor for the device that generated the event.
    /// - `display_size`: The size of the associated touch display.
    /// - `event_time`: The time in nanoseconds when the event was first recorded.
    fn create_pointer_sample_event(
        phase: pointerinjector::EventPhase,
        contact: &touch_binding::TouchContact,
        touch_descriptor: &touch_binding::TouchScreenDeviceDescriptor,
        display_size: &Size,
        event_time: zx::MonotonicInstant,
    ) -> pointerinjector::Event {
        let position =
            Self::display_coordinate_from_contact(&contact, &touch_descriptor, display_size);
        let pointer_sample = pointerinjector::PointerSample {
            pointer_id: Some(contact.id),
            phase: Some(phase),
            position_in_viewport: Some([position.x, position.y]),
            scroll_v: None,
            scroll_h: None,
            pressed_buttons: None,
            ..Default::default()
        };
        let data = pointerinjector::Data::PointerSample(pointer_sample);

        let trace_flow_id = fuchsia_trace::Id::random();
        let event = pointerinjector::Event {
            timestamp: Some(event_time.into_nanos()),
            data: Some(data),
            trace_flow_id: Some(trace_flow_id.into()),
            ..Default::default()
        };

        fuchsia_trace::flow_begin!(c"input", c"dispatch_event_to_scenic", trace_flow_id);

        event
    }

    /// Converts an input event touch to a display coordinate, which is the coordinate space in
    /// which Scenic handles events.
    ///
    /// The display coordinate is calculated by normalizing the contact position to the display
    /// size. It does not account for the viewport position, which Scenic handles directly.
    ///
    /// # Parameters
    /// - `contact`: The contact to get the display coordinate from.
    /// - `touch_descriptor`: The device descriptor for the device that generated the event.
    ///                       This is used to compute the device coordinate.
    ///
    /// # Returns
    /// (x, y) coordinates.
    fn display_coordinate_from_contact(
        contact: &touch_binding::TouchContact,
        touch_descriptor: &touch_binding::TouchScreenDeviceDescriptor,
        display_size: &Size,
    ) -> Position {
        if let Some(contact_descriptor) = touch_descriptor.contacts.first() {
            // Scale the x position.
            let x_range: f32 =
                contact_descriptor.x_range.max as f32 - contact_descriptor.x_range.min as f32;
            let x_wrt_range: f32 = contact.position.x - contact_descriptor.x_range.min as f32;
            let x: f32 = (display_size.width * x_wrt_range) / x_range;

            // Scale the y position.
            let y_range: f32 =
                contact_descriptor.y_range.max as f32 - contact_descriptor.y_range.min as f32;
            let y_wrt_range: f32 = contact.position.y - contact_descriptor.y_range.min as f32;
            let y: f32 = (display_size.height * y_wrt_range) / y_range;

            Position { x, y }
        } else {
            return contact.position;
        }
    }

    /// Watches for viewport updates from the scene manager.
    pub async fn watch_viewport(self: Rc<Self>) {
        let configuration_proxy = self.configuration_proxy.clone();
        let mut viewport_stream = HangingGetStream::new(
            configuration_proxy,
            pointerinjector_config::SetupProxy::watch_viewport,
        );
        loop {
            match viewport_stream.next().await {
                Some(Ok(new_viewport)) => {
                    // Update the viewport tracked by this handler.
                    self.mutable_state.borrow_mut().viewport = Some(new_viewport.clone());

                    // Update Scenic with the latest viewport.
                    let injectors: Vec<pointerinjector::DeviceProxy> =
                        self.mutable_state.borrow_mut().injectors.values().cloned().collect();
                    for injector in injectors {
                        let events = &[pointerinjector::Event {
                            timestamp: Some(fuchsia_async::MonotonicInstant::now().into_nanos()),
                            data: Some(pointerinjector::Data::Viewport(new_viewport.clone())),
                            trace_flow_id: Some(fuchsia_trace::Id::random().into()),
                            ..Default::default()
                        }];
                        injector.inject(events).await.expect("Failed to inject updated viewport.");
                    }
                }
                Some(Err(e)) => {
                    self.metrics_logger.log_error(
                        InputPipelineErrorMetricDimensionEvent::TouchInjectorErrorWhileReadingViewportUpdate,
                        std::format!("Error while reading viewport update: {}", e));
                    return;
                }
                None => {
                    self.metrics_logger.log_error(
                        InputPipelineErrorMetricDimensionEvent::TouchInjectorViewportUpdateStreamTerminatedUnexpectedly,
                        "Viewport update stream terminated unexpectedly");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_handler::InputHandler;
    use crate::testing_utilities::{
        create_fake_input_event, create_touch_contact, create_touch_pointer_sample_event,
        create_touch_screen_event, create_touch_screen_event_with_handled, create_touchpad_event,
        get_touch_screen_device_descriptor,
    };
    use assert_matches::assert_matches;
    use maplit::hashmap;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::convert::TryFrom as _;
    use std::ops::Add;
    use {
        fidl_fuchsia_input_report as fidl_input_report, fidl_fuchsia_ui_input as fidl_ui_input,
        fuchsia_async as fasync,
    };

    const TOUCH_ID: u32 = 1;
    const DISPLAY_WIDTH: f32 = 100.0;
    const DISPLAY_HEIGHT: f32 = 100.0;

    /// Returns an |input_device::InputDeviceDescriptor::Touchpad|.
    fn get_touchpad_device_descriptor() -> input_device::InputDeviceDescriptor {
        input_device::InputDeviceDescriptor::Touchpad(touch_binding::TouchpadDeviceDescriptor {
            device_id: 1,
            contacts: vec![touch_binding::ContactDeviceDescriptor {
                x_range: fidl_input_report::Range { min: 0, max: 100 },
                y_range: fidl_input_report::Range { min: 0, max: 100 },
                x_unit: fidl_input_report::Unit {
                    type_: fidl_input_report::UnitType::Meters,
                    exponent: -6,
                },
                y_unit: fidl_input_report::Unit {
                    type_: fidl_input_report::UnitType::Meters,
                    exponent: -6,
                },
                pressure_range: None,
                width_range: None,
                height_range: None,
            }],
        })
    }

    /// Handles |fidl_fuchsia_pointerinjector_configuration::SetupRequest::GetViewRefs|.
    async fn handle_configuration_request_stream(
        stream: &mut pointerinjector_config::SetupRequestStream,
    ) {
        if let Some(Ok(request)) = stream.next().await {
            match request {
                pointerinjector_config::SetupRequest::GetViewRefs { responder, .. } => {
                    let context = fuchsia_scenic::ViewRefPair::new()
                        .expect("Failed to create viewrefpair.")
                        .view_ref;
                    let target = fuchsia_scenic::ViewRefPair::new()
                        .expect("Failed to create viewrefpair.")
                        .view_ref;
                    let _ = responder.send(context, target);
                }
                _ => {}
            };
        }
    }

    /// Handles |fidl_fuchsia_pointerinjector::DeviceRequest|s by asserting the `injector_stream`
    /// gets `expected_event`.
    async fn handle_device_request_stream(
        mut injector_stream: pointerinjector::DeviceRequestStream,
        expected_event: pointerinjector::Event,
    ) {
        match injector_stream.next().await {
            Some(Ok(pointerinjector::DeviceRequest::Inject { events, responder })) => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].timestamp, expected_event.timestamp);
                assert_eq!(events[0].data, expected_event.data);
                responder.send().expect("failed to respond");
            }
            Some(Err(e)) => panic!("FIDL error {}", e),
            None => panic!("Expected another event."),
        }
    }

    // Creates a |pointerinjector::Viewport|.
    fn create_viewport(min: f32, max: f32) -> pointerinjector::Viewport {
        pointerinjector::Viewport {
            extents: Some([[min, min], [max, max]]),
            viewport_to_context_transform: None,
            ..Default::default()
        }
    }

    // Tests that TouchInjectorHandler::watch_viewport() tracks viewport updates and notifies
    // injectors about said updates.
    #[fuchsia::test]
    fn receives_viewport_updates() {
        let mut exec = fasync::TestExecutor::new();

        // Create touch handler.
        let (configuration_proxy, mut configuration_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector_config::SetupMarker>();
        let (injector_registry_proxy, _injector_registry_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::RegistryMarker>();
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");
        let touch_handler_fut = TouchInjectorHandler::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            Size { width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT },
            &test_node,
            metrics::MetricsLogger::default(),
        );
        let config_request_stream_fut =
            handle_configuration_request_stream(&mut configuration_request_stream);
        let (touch_handler_res, _) = exec.run_singlethreaded(futures::future::join(
            touch_handler_fut,
            config_request_stream_fut,
        ));
        let touch_handler = touch_handler_res.expect("Failed to create touch handler.");

        // Add an injector.
        let (injector_device_proxy, mut injector_device_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::DeviceMarker>();
        touch_handler.mutable_state.borrow_mut().injectors.insert(1, injector_device_proxy);

        // This nested block is used to bound the lifetime of `watch_viewport_fut`.
        {
            // Request a viewport update.
            let watch_viewport_fut = touch_handler.clone().watch_viewport();
            futures::pin_mut!(watch_viewport_fut);
            assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

            // Send a viewport update.
            match exec.run_singlethreaded(&mut configuration_request_stream.next()) {
                Some(Ok(pointerinjector_config::SetupRequest::WatchViewport {
                    responder, ..
                })) => {
                    responder.send(&create_viewport(0.0, 100.0)).expect("Failed to send viewport.");
                }
                other => panic!("Received unexpected value: {:?}", other),
            };
            assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

            // Check that the injector received an updated viewport
            exec.run_singlethreaded(async {
                match injector_device_request_stream.next().await {
                    Some(Ok(pointerinjector::DeviceRequest::Inject { events, responder })) => {
                        assert_eq!(events.len(), 1);
                        assert!(events[0].data.is_some());
                        assert_eq!(
                            events[0].data,
                            Some(pointerinjector::Data::Viewport(create_viewport(0.0, 100.0)))
                        );
                        responder.send().expect("injector stream failed to respond.");
                    }
                    other => panic!("Received unexpected value: {:?}", other),
                }
            });

            // Request viewport update.
            assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

            // Send viewport update.
            match exec.run_singlethreaded(&mut configuration_request_stream.next()) {
                Some(Ok(pointerinjector_config::SetupRequest::WatchViewport {
                    responder, ..
                })) => {
                    responder
                        .send(&create_viewport(100.0, 200.0))
                        .expect("Failed to send viewport.");
                }
                other => panic!("Received unexpected value: {:?}", other),
            };

            // Process viewport update.
            assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

            // Check that the injector received an updated viewport
            exec.run_singlethreaded(async {
                match injector_device_request_stream.next().await {
                    Some(Ok(pointerinjector::DeviceRequest::Inject { events, responder })) => {
                        assert_eq!(events.len(), 1);
                        assert!(events[0].data.is_some());
                        assert_eq!(
                            events[0].data,
                            Some(pointerinjector::Data::Viewport(create_viewport(100.0, 200.0)))
                        );
                        responder.send().expect("injector stream failed to respond.");
                    }
                    other => panic!("Received unexpected value: {:?}", other),
                }
            });
        }

        // Check the viewport on the handler is accurate.
        let expected_viewport = create_viewport(100.0, 200.0);
        assert_eq!(touch_handler.mutable_state.borrow().viewport, Some(expected_viewport));
    }

    // Tests that an add contact event is dropped without a viewport.
    #[fuchsia::test]
    fn add_contact_drops_without_viewport() {
        let mut exec = fasync::TestExecutor::new();

        // Set up fidl streams.
        let (configuration_proxy, mut configuration_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector_config::SetupMarker>();
        let (injector_registry_proxy, mut injector_registry_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::RegistryMarker>();
        let config_request_stream_fut =
            handle_configuration_request_stream(&mut configuration_request_stream);

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");

        // Create TouchInjectorHandler.
        let touch_handler_fut = TouchInjectorHandler::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            Size { width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT },
            &test_node,
            metrics::MetricsLogger::default(),
        );
        let (touch_handler_res, _) = exec.run_singlethreaded(futures::future::join(
            touch_handler_fut,
            config_request_stream_fut,
        ));
        let touch_handler = touch_handler_res.expect("Failed to create touch handler.");

        // Create touch event.
        let event_time = zx::MonotonicInstant::get();
        let contact = create_touch_contact(TOUCH_ID, Position { x: 20.0, y: 40.0 });
        let descriptor = get_touch_screen_device_descriptor();
        let input_event = input_device::UnhandledInputEvent::try_from(create_touch_screen_event(
            hashmap! {
                fidl_ui_input::PointerEventPhase::Add
                    => vec![contact.clone()],
            },
            event_time,
            &descriptor,
        ))
        .unwrap();

        // Try to handle the event.
        // Subtle: We handle the event on a clone of the handler because the call consumes the
        // handler, whose reference to `injectory_registry_proxy` is needed to keep
        // `injector_registry_request_stream` alive.
        let mut handle_event_fut = touch_handler.clone().handle_unhandled_input_event(input_event);
        let _ = exec.run_until_stalled(&mut handle_event_fut);

        // Injector should not receive anything because the handler has no viewport.
        let mut ir_fut = injector_registry_request_stream.next();
        assert_matches!(exec.run_until_stalled(&mut ir_fut), futures::task::Poll::Pending);
    }

    // Tests that an add contact event is handled correctly with a viewport.
    #[fuchsia::test]
    fn add_contact_succeeds_with_viewport() {
        let mut exec = fasync::TestExecutor::new();

        // Create touch handler.
        let (configuration_proxy, mut configuration_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector_config::SetupMarker>();
        let (injector_registry_proxy, _injector_registry_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::RegistryMarker>();
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");
        let touch_handler_fut = TouchInjectorHandler::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            Size { width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT },
            &test_node,
            metrics::MetricsLogger::default(),
        );
        let config_request_stream_fut =
            handle_configuration_request_stream(&mut configuration_request_stream);
        let (touch_handler_res, _) = exec.run_singlethreaded(futures::future::join(
            touch_handler_fut,
            config_request_stream_fut,
        ));
        let touch_handler = touch_handler_res.expect("Failed to create touch handler.");

        // Add an injector.
        let (injector_device_proxy, mut injector_device_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::DeviceMarker>();
        touch_handler.mutable_state.borrow_mut().injectors.insert(1, injector_device_proxy);

        // Request a viewport update.
        let watch_viewport_fut = fasync::Task::local(touch_handler.clone().watch_viewport());
        futures::pin_mut!(watch_viewport_fut);
        assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

        // Send a viewport update.
        match exec.run_singlethreaded(&mut configuration_request_stream.next()) {
            Some(Ok(pointerinjector_config::SetupRequest::WatchViewport { responder, .. })) => {
                responder.send(&create_viewport(0.0, 100.0)).expect("Failed to send viewport.");
            }
            other => panic!("Received unexpected value: {:?}", other),
        };
        assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

        // Check that the injector received an updated viewport
        exec.run_singlethreaded(async {
            match injector_device_request_stream.next().await {
                Some(Ok(pointerinjector::DeviceRequest::Inject { events, responder })) => {
                    assert_eq!(events.len(), 1);
                    assert!(events[0].data.is_some());
                    assert_eq!(
                        events[0].data,
                        Some(pointerinjector::Data::Viewport(create_viewport(0.0, 100.0)))
                    );
                    responder.send().expect("injector stream failed to respond.");
                }
                other => panic!("Received unexpected value: {:?}", other),
            }
        });

        // Create touch event.
        let event_time = zx::MonotonicInstant::get();
        let contact = create_touch_contact(TOUCH_ID, Position { x: 20.0, y: 40.0 });
        let descriptor = get_touch_screen_device_descriptor();
        let input_event = input_device::UnhandledInputEvent::try_from(create_touch_screen_event(
            hashmap! {
                fidl_ui_input::PointerEventPhase::Add
                    => vec![contact.clone()],
            },
            event_time,
            &descriptor,
        ))
        .unwrap();

        // Handle event.
        let handle_event_fut = touch_handler.clone().handle_unhandled_input_event(input_event);

        // Declare expected event.
        let expected_event = create_touch_pointer_sample_event(
            pointerinjector::EventPhase::Add,
            &contact,
            Position { x: 20.0, y: 40.0 },
            event_time,
        );

        // Await all futures concurrently. If this completes, then the touch event was handled and
        // matches `expected_event`.
        let device_fut =
            handle_device_request_stream(injector_device_request_stream, expected_event);
        let (handle_result, _) =
            exec.run_singlethreaded(futures::future::join(handle_event_fut, device_fut));

        // No unhandled events.
        assert_matches!(
            handle_result.as_slice(),
            [input_device::InputEvent { handled: input_device::Handled::Yes, .. }]
        );
    }

    // Tests that an add touchpad contact event with viewport is unhandled and not send to scenic.
    #[fuchsia::test]
    fn add_touchpad_contact_with_viewport() {
        let mut exec = fasync::TestExecutor::new();

        // Create touch handler.
        let (configuration_proxy, mut configuration_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector_config::SetupMarker>();
        let (injector_registry_proxy, mut injector_registry_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::RegistryMarker>();
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");
        let touch_handler_fut = TouchInjectorHandler::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            Size { width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT },
            &test_node,
            metrics::MetricsLogger::default(),
        );
        let config_request_stream_fut =
            handle_configuration_request_stream(&mut configuration_request_stream);
        let (touch_handler_res, _) = exec.run_singlethreaded(futures::future::join(
            touch_handler_fut,
            config_request_stream_fut,
        ));
        let touch_handler = touch_handler_res.expect("Failed to create touch handler.");

        // Add an injector.
        let (injector_device_proxy, mut injector_device_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::DeviceMarker>();
        touch_handler.mutable_state.borrow_mut().injectors.insert(1, injector_device_proxy);

        // Request a viewport update.
        let watch_viewport_fut = fasync::Task::local(touch_handler.clone().watch_viewport());
        futures::pin_mut!(watch_viewport_fut);
        assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

        // Send a viewport update.
        match exec.run_singlethreaded(&mut configuration_request_stream.next()) {
            Some(Ok(pointerinjector_config::SetupRequest::WatchViewport { responder, .. })) => {
                responder.send(&create_viewport(0.0, 100.0)).expect("Failed to send viewport.");
            }
            other => panic!("Received unexpected value: {:?}", other),
        };
        assert!(exec.run_until_stalled(&mut watch_viewport_fut).is_pending());

        // Check that the injector received an updated viewport
        exec.run_singlethreaded(async {
            match injector_device_request_stream.next().await {
                Some(Ok(pointerinjector::DeviceRequest::Inject { events, responder })) => {
                    assert_eq!(events.len(), 1);
                    assert!(events[0].data.is_some());
                    assert_eq!(
                        events[0].data,
                        Some(pointerinjector::Data::Viewport(create_viewport(0.0, 100.0)))
                    );
                    responder.send().expect("injector stream failed to respond.");
                }
                other => panic!("Received unexpected value: {:?}", other),
            }
        });

        // Create touch event.
        let event_time = zx::MonotonicInstant::get();
        let contact = create_touch_contact(TOUCH_ID, Position { x: 20.0, y: 40.0 });
        let descriptor = get_touchpad_device_descriptor();
        let input_event = input_device::UnhandledInputEvent::try_from(create_touchpad_event(
            vec![contact.clone()],
            HashSet::new(),
            event_time,
            &descriptor,
        ))
        .unwrap();

        // Handle event.
        let handle_event_fut = touch_handler.clone().handle_unhandled_input_event(input_event);

        let handle_result = exec.run_singlethreaded(handle_event_fut);

        // Event is not handled.
        assert_matches!(
            handle_result.as_slice(),
            [input_device::InputEvent { handled: input_device::Handled::No, .. }]
        );

        // Injector should not receive anything because the handler does not support touchpad yet.
        let mut ir_fut = injector_registry_request_stream.next();
        assert_matches!(exec.run_until_stalled(&mut ir_fut), futures::task::Poll::Pending);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn touch_injector_handler_initialized_with_inspect_node() {
        let (configuration_proxy, mut configuration_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector_config::SetupMarker>();
        let inspector = fuchsia_inspect::Inspector::default();
        let fake_handlers_node = inspector.root().create_child("input_handlers_node");
        let touch_handler_fut = TouchInjectorHandler::new_with_config_proxy(
            configuration_proxy,
            Size { width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT },
            &fake_handlers_node,
            metrics::MetricsLogger::default(),
        );
        let config_request_stream_fut =
            handle_configuration_request_stream(&mut configuration_request_stream);
        let (touch_handler_res, _) = futures::join!(touch_handler_fut, config_request_stream_fut,);
        let _handler = touch_handler_res.expect("Failed to create touch handler.");
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_handlers_node: {
                touch_injector_handler: {
                    events_received_count: 0u64,
                    events_handled_count: 0u64,
                    last_received_timestamp_ns: 0u64,
                    "fuchsia.inspect.Health": {
                        status: "STARTING_UP",
                        // Timestamp value is unpredictable and not relevant in this context,
                        // so we only assert that the property is present.
                        start_timestamp_nanos: diagnostics_assertions::AnyProperty
                    },
                }
            }
        });
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn touch_injector_handler_inspect_counts_events() {
        // Set up fidl streams.
        let (configuration_proxy, mut configuration_request_stream) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector_config::SetupMarker>();
        let (injector_registry_proxy, _) =
            fidl::endpoints::create_proxy_and_stream::<pointerinjector::RegistryMarker>();

        let inspector = fuchsia_inspect::Inspector::default();
        let fake_handlers_node = inspector.root().create_child("input_handlers_node");

        // Create touch handler.
        let touch_handler_fut = TouchInjectorHandler::new_handler(
            configuration_proxy,
            injector_registry_proxy,
            Size { width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT },
            &fake_handlers_node,
            metrics::MetricsLogger::default(),
        );
        let config_request_stream_fut =
            handle_configuration_request_stream(&mut configuration_request_stream);
        let (touch_handler_res, _) = futures::join!(touch_handler_fut, config_request_stream_fut);
        let touch_handler = touch_handler_res.expect("Failed to create touch handler");

        let contact = create_touch_contact(TOUCH_ID, Position { x: 20.0, y: 40.0 });
        let descriptor = get_touch_screen_device_descriptor();
        let event_time1 = zx::MonotonicInstant::get();
        let event_time2 = event_time1.add(zx::MonotonicDuration::from_micros(1));
        let event_time3 = event_time2.add(zx::MonotonicDuration::from_micros(1));

        let input_events = vec![
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Add
                        => vec![contact.clone()],
                },
                event_time1,
                &descriptor,
            ),
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Move
                        => vec![contact.clone()],
                },
                event_time2,
                &descriptor,
            ),
            // Should not count non-touch input event.
            create_fake_input_event(event_time2),
            // Should not count received event that has already been handled.
            create_touch_screen_event_with_handled(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Move
                        => vec![contact.clone()],
                },
                event_time2,
                &descriptor,
                input_device::Handled::Yes,
            ),
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Remove
                        => vec![contact.clone()],
                },
                event_time3,
                &descriptor,
            ),
        ];

        for input_event in input_events {
            touch_handler.clone().handle_input_event(input_event).await;
        }

        let last_received_event_time: u64 = event_time3.into_nanos().try_into().unwrap();

        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_handlers_node: {
                touch_injector_handler: {
                    events_received_count: 3u64,
                    events_handled_count: 3u64,
                    last_received_timestamp_ns: last_received_event_time,
                    "fuchsia.inspect.Health": {
                        status: "STARTING_UP",
                        // Timestamp value is unpredictable and not relevant in this context,
                        // so we only assert that the property is present.
                        start_timestamp_nanos: diagnostics_assertions::AnyProperty
                    },
                }
            }
        });
    }
}
