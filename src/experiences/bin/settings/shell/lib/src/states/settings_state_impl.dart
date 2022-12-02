// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(http://fxbug.dev/107480): Resolve lint issues and reenable analysis for file
// ignore_for_file: cascade_invocations

import 'dart:io';

import 'package:ermine_utils/ermine_utils.dart';
import 'package:flutter/material.dart' hide Action;
import 'package:flutter/services.dart';
import 'package:internationalization/strings.dart';
import 'package:intl/intl.dart';
import 'package:mobx/mobx.dart';
import 'package:shell_settings/src/services/brightness_service.dart';
import 'package:shell_settings/src/services/channel_service.dart';
import 'package:shell_settings/src/services/datetime_service.dart';
import 'package:shell_settings/src/services/task_service.dart';
import 'package:shell_settings/src/services/timezone_service.dart';
import 'package:shell_settings/src/states/settings_state.dart';

/// Defines the implementation of [SettingsState].
class SettingsStateImpl with Disposable implements SettingsState, TaskService {
  static const kTimezonesFile = '/pkg/data/tz_ids.txt';

  // All
  final settingsPage = SettingsPage.none.asObservable();

  @override
  bool get allSettingsPageVisible => _allSettingsPageVisible.value;
  late final _allSettingsPageVisible =
      (() => settingsPage.value == SettingsPage.none).asComputed();

  // Timezone
  @override
  bool get timezonesPageVisible => _timezonesPageVisible.value;
  late final _timezonesPageVisible =
      (() => settingsPage.value == SettingsPage.timezone).asComputed();

  @override
  String get selectedTimezone => _selectedTimezone.value;
  set selectedTimezone(String value) => _selectedTimezone.value = value;
  final Observable<String> _selectedTimezone;

  final List<String> _timezones;

  @override
  List<String> get timezones {
    // Move the selected timezone to the top.
    return [selectedTimezone]
      ..addAll(_timezones.where((zone) => zone != selectedTimezone));
  }

  // Datetime
  @override
  String get dateTime => _dateTime.value;
  late final ObservableValue<String> _dateTime = (() =>
      // Ex: Mon, Jun 7 2:25 AM
      DateFormat.MMMEd().add_jm().format(dateTimeNow.value)).asComputed();

  // Brightness
  @override
  double? get brightnessLevel => _brightnessLevel.value;
  set brightnessLevel(double? value) => _brightnessLevel.value = value;
  final Observable<double?> _brightnessLevel = Observable<double?>(null);

  @override
  bool? get brightnessAuto => _brightnessAuto.value;
  set brightnessAuto(bool? value) => _brightnessAuto.value = value;
  final Observable<bool?> _brightnessAuto = Observable<bool?>(null);

  @override
  IconData get brightnessIcon => _brightnessIcon.value;
  set brightnessIcon(IconData value) => _brightnessIcon.value = value;
  final Observable<IconData> _brightnessIcon =
      Icons.brightness_auto.asObservable();

  // Channel
  @override
  bool get channelPageVisible => _channelPageVisible.value;
  late final _channelPageVisible =
      (() => settingsPage.value == SettingsPage.channel).asComputed();

  @override
  String get currentChannel => _currentChannel.value;
  set currentChannel(String value) => _currentChannel.value = value;
  final Observable<String> _currentChannel = Observable<String>('');

  @override
  final List<String> availableChannels = ObservableList<String>();

  @override
  String get targetChannel => _targetChannel.value;
  set targetChannel(String value) => _targetChannel.value = value;
  final Observable<String> _targetChannel = Observable<String>('');

  @override
  ChannelState get channelState => _channelState.value;
  set channelState(ChannelState value) => _channelState.value = value;
  final _channelState = Observable<ChannelState>(ChannelState.idle);

  @override
  bool? get optedIntoUpdates => _optedIntoUpdates.value;
  set optedIntoUpdates(bool? value) => _optedIntoUpdates.value = value;
  final Observable<bool?> _optedIntoUpdates = Observable<bool?>(null);

  @override
  double get systemUpdateProgress => _systemUpdateProgress.value;
  set systemUpdateProgress(double value) => _systemUpdateProgress.value = value;
  final _systemUpdateProgress = Observable<double>(0);

  // Services
  final BrightnessService brightnessService;
  final DateTimeService dateTimeService;
  final TimezoneService timezoneService;
  final ChannelService channelService;

  // Constructor
  SettingsStateImpl({
    required this.dateTimeService,
    required this.timezoneService,
    required this.brightnessService,
    required this.channelService,
  })  : _timezones = _loadTimezones(),
        _selectedTimezone = timezoneService.timezone.asObservable() {
    dateTimeService.onChanged = updateDateTime;
    timezoneService.onChanged =
        (timezone) => runInAction(() => selectedTimezone = timezone);
    brightnessService.onChanged = () {
      runInAction(() {
        brightnessLevel = brightnessService.brightness;
        brightnessAuto = brightnessService.auto;
        brightnessIcon = brightnessService.icon;
      });
    };
    channelService.onChanged = () {
      runInAction(() {
        optedIntoUpdates = channelService.optedIntoUpdates;
        currentChannel = channelService.currentChannel;
        systemUpdateProgress = channelService.updateProgress;
        List<String> channels;
        // FIDL messages are unmodifiable so we need to copy the list of
        // channels here.
        channels = channelService.channels.toList();
        // Sort channels for readability
        channels.sort();
        availableChannels
          ..clear()
          ..addAll(channels);
        targetChannel = channelService.targetChannel;
        // Monitor state of update
        if (channelService.checkingForUpdates) {
          channelState = ChannelState.checkingForUpdates;
        } else if (channelService.errorCheckingForUpdate) {
          channelState = ChannelState.errorCheckingForUpdate;
        } else if (channelService.noUpdateAvailable) {
          channelState = ChannelState.noUpdateAvailable;
        } else if (channelService.installationDeferredByPolicy) {
          channelState = ChannelState.installationDeferredByPolicy;
        } else if (channelService.installingUpdate) {
          channelState = ChannelState.installingUpdate;
        } else if (channelService.waitingForReboot) {
          channelState = ChannelState.waitingForReboot;
        } else if (channelService.installationError) {
          channelState = ChannelState.installationError;
        }
      });
    };

    // We cannot load MaterialIcons font file from pubspec.yaml. So load it
    // explicitly.
    File file = File('/pkg/data/MaterialIcons-Regular.otf');
    if (file.existsSync()) {
      FontLoader('MaterialIcons')
        ..addFont(() async {
          return file.readAsBytesSync().buffer.asByteData();
        }())
        ..load();
    }
  }

  // Task service
  @override
  Future<void> start() async {
    await Future.wait([
      dateTimeService.start(),
      timezoneService.start(),
      brightnessService.start(),
      channelService.start(),
    ]);
  }

  @override
  Future<void> stop() async {
    showAllSettings();
    await dateTimeService.stop();
    await timezoneService.stop();
    await brightnessService.stop();
    await channelService.stop();
    _dateTimeNow = null;
  }

  @override
  void dispose() {
    super.dispose();
    dateTimeService.dispose();
    timezoneService.dispose();
    brightnessService.dispose();
    channelService.dispose();
  }

  // All
  @override
  void showAllSettings() =>
      runInAction(() => settingsPage.value = SettingsPage.none);

  // Timezone
  @override
  void updateTimezone(String timezone) => runInAction(() {
        selectedTimezone = timezone;
        timezoneService.timezone = timezone;
        settingsPage.value = SettingsPage.none;
      });

  @override
  void showTimezoneSettings() =>
      runInAction(() => settingsPage.value = SettingsPage.timezone);

  static List<String> _loadTimezones() {
    return File(kTimezonesFile).readAsLinesSync();
  }

  // Datetime
  Observable<DateTime>? _dateTimeNow;
  Observable<DateTime> get dateTimeNow =>
      _dateTimeNow ??= DateTime.now().asObservable();

  late final Action updateDateTime = () {
    dateTimeNow.value = DateTime.now();
  }.asAction();

  // Brightness
  @override
  void setBrightnessLevel(double value) =>
      runInAction(() => brightnessService.brightness = value);

  @override
  void increaseBrightness() =>
      runInAction(brightnessService.increaseBrightness);

  @override
  void decreaseBrightness() =>
      runInAction(brightnessService.decreaseBrightness);

  @override
  void setBrightnessAuto() => runInAction(() => brightnessService.auto = true);

  // Channel
  @override
  void showChannelSettings() =>
      runInAction(() => settingsPage.value = SettingsPage.channel);

  @override
  void setTargetChannel(String value) =>
      runInAction(() => channelService.targetChannel = value);

  @override
  void checkForUpdates() => runInAction(channelService.checkForUpdates);
}
