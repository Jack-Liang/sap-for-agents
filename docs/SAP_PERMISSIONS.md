# SAP Authorization Configuration (L1: Minimum Privileges for the Gateway Account)

[English](./SAP_PERMISSIONS.md) | [简体中文](./SAP_PERMISSIONS.zh-CN.md)

## Why It Matters

The sap-for-agents gateway connects to SAP using the `SAP_USER` defined in `.env`. **What the gateway can call = the privileges granted to that account.**

By default it uses `DEVELOPER` (a developer account with broad privileges). This means anyone who can reach the gateway can call every function that `DEVELOPER` can invoke—reading master data, changing orders, creating users, and so on.

**The right approach**: create a dedicated, restricted account and grant it only the Function Group authorizations the business actually needs. This way, if the gateway is abused or a token leaks, the blast radius is limited to that account's privileges (a minimum).

> This is more effective than maintaining a function allowlist at the gateway layer. SAP's authorization system (PFCG / `S_RFC`) is purpose-built for this, with mature tooling.

## Configuration Steps (Basis Administration)

### 1. Create a Dedicated Service Account (SU01)

- In transaction `SU01`, create a new user (e.g. `SAP_RFC_GW`).
- User type: **System** (a system user that cannot log on interactively; intended for RFC).
- Initial password: set a strong password (record it for `.env`).

### 2. Create a Role (PFCG)

- In transaction `PFCG`, create a new role (e.g. `Z_RFC_GW_READONLY`).
- On the **Authorizations** tab, add authorization object **`S_RFC`**:
  - `ACTVT` = `16` (Execute RFC)
  - `RFC_TYPE` = `FUNC` (Function Group)
  - `RFC_NAME` = <allowed function groups, e.g. `BAPI_USER_BANK`, `SRFC`, or custom `Z*` groups>
  - **Do not use `*`** (full authorization; equivalent to DEVELOPER).

> `RFC_NAME` accepts multiple values. Apply **least privilege** based on business needs—list only the function groups you will actually call.
>
> Common read-only groups (as needed): `SZRP` (RFC metadata, required by the gateway's metadata endpoints), `SUPI`/`SUSR` (user-related BAPIs), and others.

### 3. Assign the Role to the Account

- In `SU01`, assign the `Z_RFC_GW_READONLY` role to `SAP_RFC_GW`.
- In `PFCG`, generate the role's profile.

### 4. Update `.env`

```env
SAP_USER=SAP_RFC_GW
SAP_PASSWD=<new account password>
```

Restart the gateway for the change to take effect.

### 5. Verify

- Call an **authorized** function (e.g. `BAPI_USER_GETLIST`, if it is in an authorized group) → `200`.
- Call an **unauthorized** function (e.g. a finance BAPI not in any authorized group) → `403 {"error":{"code":403,"key":"RFC_AUTHORIZATION_FAILURE"}}`.

An unauthorized call returning `403` confirms the privilege boundary is in effect—SAP handles the allowlist enforcement for you.

## Why Not Build an Allowlist at the Gateway Layer

- SAP ships tens of thousands of functions; a per-function allowlist is unmaintainable and always leaks.
- SAP's authorization system (`S_RFC`) aggregates by **function group** (a few dozen rules vs. thousands of functions), with a granularity aligned to business capabilities.
- A gateway-layer allowlist is an optional "second gate" (L2, filtering by group prefix). L1 (SAP authorizations) is the **first and most effective** layer.

> Note: the gateway currently does NOT implement an L2 filter; the privilege boundary rests entirely on SAP-side authorizations.
