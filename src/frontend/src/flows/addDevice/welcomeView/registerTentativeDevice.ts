import { WebAuthnIdentity } from "@dfinity/identity";
import { html } from "lit-html";
import {
  AddTentativeDeviceResponse,
  CredentialId,
  DeviceData,
} from "../../../../generated/internet_identity_types";
import { promptDeviceAlias } from "../../../components/alias";
import { displayError } from "../../../components/displayError";
import { withLoader } from "../../../components/loader";
import { authenticatorAttachmentToKeyType } from "../../../utils/authenticatorAttachment";
import { Connection, creationOptions } from "../../../utils/iiConnection";
import {
  unknownToString,
  unreachable,
  unreachableLax,
} from "../../../utils/utils";
import { deviceRegistrationDisabledInfo } from "./deviceRegistrationModeDisabled";
import { showVerificationCode } from "./showVerificationCode";

/**
 * Prompts the user to enter a device alias. When clicking next, the device is added tentatively to the given identity anchor.
 * @param userNumber anchor to add the tentative device to.
 */
export const registerTentativeDevice = async (
  userNumber: bigint,
  connection: Connection
): Promise<"ok"> => {
  // First, we need an alias for the device to (tentatively) add
  const alias = await promptDeviceAlias({
    title: "Add a Trusted Device",
    message: html` What device do you want to add to anchor
      <strong class="t-strong">${userNumber}</strong>?`,
  });

  if (alias === null) {
    // TODO L2-309: do this without reload
    return window.location.reload() as never;
  }

  // Then, we create local WebAuthn credentials for the device
  const result = await withLoader(() =>
    createDevice({ userNumber, connection })
  );

  if (result instanceof Error) {
    await displayError({
      title: "Error adding new device",
      message: "Unable to register new WebAuthn Device.",
      detail: result.message,
      primaryButton: "Ok",
    });
    // TODO L2-309: do this without reload
    return window.location.reload() as never;
  }

  // Finally, we submit it to the canister
  const device: Omit<DeviceData, "origin"> & { credential_id: [CredentialId] } =
    {
      alias: alias,
      protection: { unprotected: null },
      pubkey: Array.from(new Uint8Array(result.getPublicKey().toDer())),
      key_type: authenticatorAttachmentToKeyType(
        result.getAuthenticatorAttachment()
      ),
      purpose: { authentication: null },
      credential_id: [Array.from(new Uint8Array(result.rawId))],
      metadata: [],
    };
  const addResponse = await addTentativeDevice({
    userNumber,
    connection,
    device,
  });

  // If everything went well we can now ask the user to authenticate on an existing device
  // and enter a verification code
  return await showVerificationCode(
    userNumber,
    connection,
    device.alias,
    addResponse.added_tentatively,
    device.credential_id[0]
  );
};

/** Create new WebAuthn credentials */
const createDevice = async ({
  userNumber,
  connection,
}: {
  userNumber: bigint;
  connection: Connection;
}): Promise<WebAuthnIdentity | Error> => {
  const existingAuthenticators = await connection.lookupAuthenticators(
    userNumber
  );
  try {
    return await WebAuthnIdentity.create({
      publicKey: creationOptions(existingAuthenticators),
    });
  } catch (error: unknown) {
    if (error instanceof Error) {
      return error;
    } else {
      return new Error(unknownToString(error, "unknown error"));
    }
  }
};

type AddDeviceSuccess = Extract<
  AddTentativeDeviceResponse,
  { added_tentatively: unknown }
>;

/** Add the device tentatively to the canister */
export const addTentativeDevice = async ({
  userNumber,
  connection,
  device,
}: {
  userNumber: bigint;
  connection: Connection;
  device: Omit<DeviceData, "origin">;
}): Promise<AddDeviceSuccess> => {
  // Try to add the device tentatively, retrying if necessary
  for (;;) {
    const result = await withLoader(() =>
      connection.addTentativeDevice(userNumber, device)
    );

    if ("another_device_tentatively_added" in result) {
      // User already added a device so we show an error and abort
      await displayError({
        title: "Tentative Device Already Exists",
        message:
          'The "add device" process was already started for another device. If you want to add this device instead, log in using an existing device and restart the "add device" process.',
        primaryButton: "Ok",
      });
      // TODO L2-309: do this without reload
      return window.location.reload() as never;
    }

    if ("device_registration_mode_off" in result) {
      // User hasn't started the "add device" flow, so we offer to enable it and retry, or cancel
      const res = await deviceRegistrationDisabledInfo(userNumber);
      if (res === "canceled") {
        // TODO L2-309: do this without reload
        return window.location.reload() as never;
      }

      if (res === "retry") {
        continue;
      }

      // We should never get here, but just in case we retry
      unreachableLax(res);
      continue;
    }

    if ("added_tentatively" in result) {
      return result;
    }

    // We should never get here, but just in case we show an error
    unreachable(result);
    break;
  }
};
