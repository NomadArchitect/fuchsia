// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "aml-usb-phy-visitor.h"

#include <lib/ddk/metadata.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>

#include <cstdint>
#include <vector>

#include <usb/usb.h>

#include "src/devices/lib/amlogic/include/soc/aml-common/aml-usb-phy.h"

namespace aml_usb_phy_visitor_dt {

AmlUsbPhyVisitor::AmlUsbPhyVisitor()
    : fdf_devicetree::DriverVisitor({"amlogic,g12a-usb-phy", "amlogic,g12b-usb-phy"}) {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(std::make_unique<fdf_devicetree::StringListProperty>(kDrModes, true));
  properties.emplace_back(std::make_unique<fdf_devicetree::StringListProperty>(kRegNames, true));
  properties.emplace_back(std::make_unique<fdf_devicetree::StringListProperty>(kCompatible, true));
  parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

zx::result<> AmlUsbPhyVisitor::DriverVisit(fdf_devicetree::Node& node,
                                           const devicetree::PropertyDecoder& decoder) {
  zx::result parser_output = parser_->Parse(node);
  if (parser_output.is_error()) {
    FDF_LOG(ERROR, "Aml usb phy visitor parse failed for node '%s' : %s", node.name().c_str(),
            parser_output.status_string());
    return parser_output.take_error();
  }

  PhyType phy_type;
  if (*parser_output->at(kCompatible)[0].AsStringList().value().begin() == "amlogic,g12a-usb-phy") {
    phy_type = kG12A;
  }
  if (*parser_output->at(kCompatible)[0].AsStringList().value().begin() == "amlogic,g12b-usb-phy") {
    phy_type = kG12B;
  } else {
    FDF_LOG(ERROR, "Node '%s' has invalid compatible string. Cannot determine PHY type. ",
            node.name().c_str());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fuchsia_hardware_platform_bus::Metadata type_metadata = {{
      .id = std::to_string(DEVICE_METADATA_PRIVATE_PHY_TYPE | DEVICE_METADATA_PRIVATE),
      .data = std::vector<uint8_t>(reinterpret_cast<const uint8_t*>(&phy_type),
                                   reinterpret_cast<const uint8_t*>(&phy_type) + sizeof(phy_type)),
  }};
  node.AddMetadata(std::move(type_metadata));

  if (parser_output->at(kRegNames).size() - 1 != parser_output->at(kDrModes).size()) {
    FDF_LOG(
        ERROR,
        "Node '%s' does not have entries in dr_modes for each PHY device. Expected - %zu, Actual - %zu.",
        node.name().c_str(), parser_output->at(kRegNames).size() - 1,
        parser_output->at(kDrModes).size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  uint32_t reg_name_index = 1;
  std::vector<UsbPhyMode> phy_modes;
  for (auto& mode : parser_output->at(kDrModes)) {
    UsbPhyMode phy_mode = {};
    auto mode_string = mode.AsString();
    // TODO:: Return error in property parse if the output is not what is expected. Maybe best to
    // never return optional value as it is confusing as to whether the caller should check it or
    // not.

    if (*mode_string == "host") {
      phy_mode.dr_mode = UsbMode::Host;
    } else if (*mode_string == "peripheral") {
      phy_mode.dr_mode = UsbMode::Peripheral;
    } else if (*mode_string == "otg") {
      phy_mode.dr_mode = UsbMode::Otg;
    }

    auto phy_name = parser_output->at(kRegNames)[reg_name_index++].AsString();
    if (*phy_name == "usb2-phy") {
      phy_mode.protocol = UsbProtocol::Usb2_0;
      phy_mode.is_otg_capable = false;
    } else if (*phy_name == "usb2-otg-phy") {
      phy_mode.protocol = UsbProtocol::Usb2_0;
      phy_mode.is_otg_capable = true;
    } else if (*phy_name == "usb3-phy") {
      phy_mode.protocol = UsbProtocol::Usb3_0;
      phy_mode.is_otg_capable = false;
    }

    phy_modes.emplace_back(phy_mode);
  }

  fuchsia_hardware_platform_bus::Metadata mode_metadata = {{
      .id = std::to_string(DEVICE_METADATA_USB_MODE),
      .data = std::vector<uint8_t>(reinterpret_cast<const uint8_t*>(phy_modes.data()),
                                   reinterpret_cast<const uint8_t*>(phy_modes.data()) +
                                       phy_modes.size() * sizeof(UsbPhyMode)),
  }};
  node.AddMetadata(std::move(mode_metadata));
  FDF_LOG(DEBUG, "Added %zu usb modes metadata to node '%s'.", phy_modes.size(),
          node.name().c_str());

  return zx::ok();
}

}  // namespace aml_usb_phy_visitor_dt

REGISTER_DEVICETREE_VISITOR(aml_usb_phy_visitor_dt::AmlUsbPhyVisitor);
