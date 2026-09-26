"use client";

import { useState } from "react";
import { Copy, Plus, LogOut } from "lucide-react";
import { SignOutButton } from "@clerk/nextjs";
import { Button, Input, SettingCard, Badge } from "@/components/ui";
import { WithIdentity } from "@/components/identity";

const clerkOn = !!process.env.NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY;

export default function AccountPage() {
  const [name, setName] = useState("");
  const [username, setUsername] = useState("dylan");
  const userId = "usr_2hX9KOptQ92LV1rMKZH6XDCXj";

  return (
    <div className="mx-auto max-w-3xl">
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-2xl font-semibold tracking-tight">Account Settings</h1>
        {clerkOn ? (
          <SignOutButton>
            <Button variant="outline"><LogOut className="h-4 w-4" /> Sign out</Button>
          </SignOutButton>
        ) : null}
      </div>
      <div className="flex flex-col gap-6">
        <SettingCard
          title="Avatar"
          desc={<>This is your avatar.<br />Click on the avatar to upload a custom one from your files.</>}
          footer="An avatar is optional but strongly recommended."
        >
          <div className="flex justify-end">
            <WithIdentity>{(id) => id.imageUrl
              // eslint-disable-next-line @next/next/no-img-element
              ? <img src={id.imageUrl} alt="" loading="lazy" decoding="async" width={64} height={64} className="h-16 w-16 rounded-full object-cover" />
              : <div className="flex h-16 w-16 items-center justify-center rounded-full bg-[#0761d1] text-xl font-semibold text-white">{id.initial}</div>}</WithIdentity>
          </div>
        </SettingCard>

        <SettingCard
          title="Display Name"
          desc="Please enter your full name, or a display name you are comfortable with."
          footer="Please use 32 characters at maximum."
          footerAction={<Button>Save</Button>}
        >
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="Your name" className="max-w-sm" />
        </SettingCard>

        <SettingCard
          title="Username"
          desc="This is your URL namespace within DevHub."
          footer="Please use 48 characters at maximum."
          footerAction={<Button>Save</Button>}
        >
          <div className="flex max-w-sm overflow-hidden rounded-md border border-border">
            <span className="flex items-center bg-subtle px-3 text-sm text-muted">openedge.cloud/</span>
            <input value={username} onChange={(e) => setUsername(e.target.value)} className="w-full bg-card px-3 py-2 text-sm focus:outline-none" />
          </div>
        </SettingCard>

        <SettingCard
          title="Default Team"
          desc="Your default team is used when you make a request through the API or CLI without specifying a team."
          footer="Learn more about Default Teams"
          footerAction={<Button variant="outline">Save</Button>}
        >
          <div className="flex max-w-sm items-center gap-2 rounded-md border border-border px-3 py-2">
            <span className="h-5 w-5 rounded-full bg-gradient-to-br from-emerald-400 to-green-600" />
            <span className="text-sm font-medium">blockoffset</span>
          </div>
        </SettingCard>

        <SettingCard
          title="Email"
          desc="Enter the email addresses you want to use to log in. Your primary email is used for account-related notifications."
          footer="Emails must be verified to be able to login with them or be used as primary email."
        >
          <WithIdentity>{(id) => (
            <div className="rounded-md border border-border px-4 py-3 text-sm">
              <div className="flex items-center gap-2">
                {id.email || "—"} <Badge tone="blue">Verified</Badge> <Badge tone="green">Primary</Badge>
              </div>
            </div>
          )}</WithIdentity>
          <div className="mt-3"><Button variant="outline"><Plus className="h-4 w-4" /> Add Another</Button></div>
        </SettingCard>

        <SettingCard
          title="Your Phone Number"
          desc="Enter a phone number to receive important service updates by SMS."
          footer="A code will be sent to verify."
          footerAction={<Button>Save</Button>}
        >
          <Input placeholder="(201) 555-0123" className="max-w-sm" />
        </SettingCard>

        <SettingCard title="User ID" desc="This is your user ID within DevHub." footer="Used when interacting with the DevHub API.">
          <button
            onClick={() => navigator.clipboard?.writeText(userId)}
            className="flex max-w-sm items-center gap-2 rounded-md border border-border bg-subtle px-3 py-2 font-mono text-sm hover:bg-card"
          >
            {userId} <Copy className="h-3.5 w-3.5 text-muted" />
          </button>
        </SettingCard>

        <SettingCard title="Reset Tips" desc="See onboarding tips you might have missed." footer="Resetting will make all onboarding tips re-appear." footerAction={<Button variant="outline">Reset</Button>} />

        <div className="rounded-xl border border-red-500/30 bg-red-500/[0.03]">
          <div className="p-5 sm:p-6">
            <h3 className="text-lg font-semibold text-red-600 dark:text-red-400">Delete Account</h3>
            <p className="mt-1.5 text-sm text-secondary">Permanently remove your account and all of its contents. This action is not reversible, so continue with caution.</p>
          </div>
          <div className="flex justify-end border-t border-red-500/20 bg-red-500/[0.04] px-5 py-3 sm:px-6">
            <Button variant="danger">Delete Account</Button>
          </div>
        </div>
      </div>
    </div>
  );
}
