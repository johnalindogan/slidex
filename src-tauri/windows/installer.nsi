Unicode true
ManifestDPIAware true
; Add in `dpiAwareness` `PerMonitorV2` to manifest for Windows 10 1607+ (note this should not affect lower versions since they should be able to ignore this and pick up `dpiAware` `true` set by `ManifestDPIAware true`)
; Currently undocumented on NSIS's website but is in the Docs folder of source tree, see
; https://github.com/kichik/nsis/blob/5fc0b87b819a9eec006df4967d08e522ddd651c9/Docs/src/attributes.but#L286-L300
; https://github.com/tauri-apps/tauri/pull/10106
ManifestDPIAwareness PerMonitorV2

!if "{{compression}}" == "none"
 SetCompress off
!else
 ; Set the compression algorithm. We default to LZMA.
 SetCompressor /SOLID "{{compression}}"
!endif

; Keep above !include to stay ahead of any plugin command
; see https://github.com/tauri-apps/tauri/pull/15422#discussion_r3289239624
{{#if signed_plugins_path}}
!addplugindir "{{signed_plugins_path}}"
{{/if}}

!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include WordFunc.nsh
!include "utils.nsh"
!include "FileAssociation.nsh"
!include "Win\COM.nsh"
!include "Win\Propkey.nsh"
!include "StrFunc.nsh"
${StrCase}
${StrLoc}

{{#if installer_hooks}}
!include "{{installer_hooks}}"
{{/if}}

!define WEBVIEW2APPGUID "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

!define MANUFACTURER "{{manufacturer}}"
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define HOMEPAGE "{{homepage}}"
!define INSTALLMODE "{{install_mode}}"
!define LICENSE "{{license}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"
!define UNINSTALLERICON "{{uninstaller_icon}}"
!define UNINSTALLERHEADERIMAGE "{{uninstaller_header_image}}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define BUNDLEID "{{bundle_id}}"
!define COPYRIGHT "{{copyright}}"
!define OUTFILE "{{out_file}}"
!define ARCH "{{arch}}"
!define ADDITIONALPLUGINSPATH "{{additional_plugins_path}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define INSTALLWEBVIEW2MODE "{{install_webview2_mode}}"
!define WEBVIEW2INSTALLERARGS "{{webview2_installer_args}}"
!define WEBVIEW2BOOTSTRAPPERPATH "{{webview2_bootstrapper_path}}"
!define WEBVIEW2INSTALLERPATH "{{webview2_installer_path}}"
!define MINIMUMWEBVIEW2VERSION "{{minimum_webview2_version}}"
; 0.1.6 registered uninstall + install-dir under SlideShowX. Keep that key so
; 0.1.7 upgrades in place. PRODUCTNAME (SlideX) is the display name, shortcut
; name, and Apps & Features DisplayName. The Tauri identifier is unchanged.
!define UPGRADEPRODUCTNAME "SlideShowX"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${UPGRADEPRODUCTNAME}"
!define MANUKEY "Software\${MANUFACTURER}"
!define MANUPRODUCTKEY "${MANUKEY}\${UPGRADEPRODUCTNAME}"
!define UNINSTALLERSIGNCOMMAND "{{uninstaller_sign_cmd}}"
!define ESTIMATEDSIZE "{{estimated_size}}"
!define STARTMENUFOLDER "{{start_menu_folder}}"

Var PassiveMode
Var UpdateMode
Var NoShortcutMode
Var WixMode
Var OldMainBinaryName

Name "${PRODUCTNAME}"
BrandingText "${COPYRIGHT}"
OutFile "${OUTFILE}"

; We don't actually use this value as default install path,
; it's just for nsis to append the product name folder in the directory selector
; https://nsis.sourceforge.io/Reference/InstallDir
!define PLACEHOLDER_INSTALL_DIR "placeholder\${PRODUCTNAME}"
InstallDir "${PLACEHOLDER_INSTALL_DIR}"

VIProductVersion "${VERSIONWITHBUILD}"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${PRODUCTNAME}"
VIAddVersionKey "LegalCopyright" "${COPYRIGHT}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

# additional plugins
!addplugindir "${ADDITIONALPLUGINSPATH}"

; Uninstaller signing command
!if "${UNINSTALLERSIGNCOMMAND}" != ""
 !uninstfinalize '${UNINSTALLERSIGNCOMMAND}'
!endif

; Handle install mode, `perUser`, `perMachine` or `both`
!if "${INSTALLMODE}" == "perMachine"
 RequestExecutionLevel admin
!endif

!if "${INSTALLMODE}" == "currentUser"
 RequestExecutionLevel user
!endif

; Cast media server listens on TCP 47200-47215. Keep in sync with CAST_PORT_LO/HI in src/cast.rs.
!define CAST_FW_TCP "47200-47215"
!define CAST_FW_UDP "5353"
!define CAST_FW_MEDIA_NAME "SlideX Cast media (Private)"
!define CAST_FW_MDNS_NAME "SlideX Cast mDNS (Private)"

!if "${INSTALLMODE}" == "both"
 !define MULTIUSER_MUI
 !define MULTIUSER_INSTALLMODE_INSTDIR "${PRODUCTNAME}"
 !define MULTIUSER_INSTALLMODE_COMMANDLINE
 !if "${ARCH}" == "x64"
 !define MULTIUSER_USE_PROGRAMFILES64
 !else if "${ARCH}" == "arm64"
 !define MULTIUSER_USE_PROGRAMFILES64
 !endif
 !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_KEY "${UNINSTKEY}"
 !define MULTIUSER_INSTALLMODE_DEFAULT_REGISTRY_VALUENAME "CurrentUser"
 !define MULTIUSER_INSTALLMODEPAGE_SHOWUSERNAME
 !define MULTIUSER_INSTALLMODE_FUNCTION RestorePreviousInstallLocation
 !define MULTIUSER_EXECUTIONLEVEL Highest
 !include MultiUser.nsh
!endif

; Installer icon
!if "${INSTALLERICON}" != ""
 !define MUI_ICON "${INSTALLERICON}"
!endif

; Installer sidebar image
!if "${SIDEBARIMAGE}" != ""
 !define MUI_WELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"
!endif

; Enable header images for installer and uninstaller pages when either image is configured.
!if "${HEADERIMAGE}" != ""
 !define MUI_HEADERIMAGE
!else if "${UNINSTALLERHEADERIMAGE}" != ""
 !define MUI_HEADERIMAGE
!endif

; Installer header image
!if "${HEADERIMAGE}" != ""
 !define MUI_HEADERIMAGE_BITMAP "${HEADERIMAGE}"
!endif

; Uninstaller header image
!if "${UNINSTALLERHEADERIMAGE}" != ""
 !define MUI_HEADERIMAGE_UNBITMAP "${UNINSTALLERHEADERIMAGE}"
!endif

; Uninstaller icon
!if "${UNINSTALLERICON}" != ""
 !define MUI_UNICON "${UNINSTALLERICON}"
!endif

; Define registry key to store installer language
!define MUI_LANGDLL_REGISTRY_ROOT "HKCU"
!define MUI_LANGDLL_REGISTRY_KEY "${MANUPRODUCTKEY}"
!define MUI_LANGDLL_REGISTRY_VALUENAME "Installer Language"

; Installer pages, must be ordered as they appear
; 1. Welcome Page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_WELCOME

; 2. License Page (if defined)
!if "${LICENSE}" != ""
 !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
 !insertmacro MUI_PAGE_LICENSE "${LICENSE}"
!endif

; 3. Install mode (if it is set to `both`)
!if "${INSTALLMODE}" == "both"
 !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
 !insertmacro MULTIUSER_PAGE_INSTALLMODE
!endif

; 4. Custom page to ask user if he wants to reinstall/uninstall
; only if a previous installation was detected
Var ReinstallPageCheck
Page custom PageReinstall PageLeaveReinstall
Function PageReinstall
 ; Uninstall previous WiX installation if exists.
 ;
 ; A WiX installer stores the installation info in registry
 ; using a UUID and so we have to loop through all keys under
 ; `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`
 ; and check if `DisplayName` and `Publisher` keys match ${PRODUCTNAME} and ${MANUFACTURER}
 ;
 ; This has a potential issue that there maybe another installation that matches
 ; our ${PRODUCTNAME} and ${MANUFACTURER} but wasn't installed by our WiX installer,
 ; however, this should be fine since the user will have to confirm the uninstallation
 ; and they can chose to abort it if doesn't make sense.
 StrCpy $0 0
 wix_loop:
 EnumRegKey $1 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall" $0
 StrCmp $1 "" wix_loop_done ; Exit loop if there is no more keys to loop on
 IntOp $0 $0 + 1
 ReadRegStr $R0 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1" "DisplayName"
 ReadRegStr $R1 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1" "Publisher"
 StrCmp "$R0$R1" "${PRODUCTNAME}${MANUFACTURER}" 0 wix_loop
 ReadRegStr $R0 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1" "UninstallString"
 ${StrCase} $R1 $R0 "L"
 ${StrLoc} $R0 $R1 "msiexec" ">"
 StrCmp $R0 0 0 wix_loop_done
 StrCpy $WixMode 1
 StrCpy $R6 "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1"
 Goto compare_version
 wix_loop_done:

 ; Check if there is an existing installation, if not, abort the reinstall page
 ReadRegStr $R0 SHCTX "${UNINSTKEY}" ""
 ReadRegStr $R1 SHCTX "${UNINSTKEY}" "UninstallString"
 ${IfThen} "$R0$R1" == "" ${|} Abort ${|}

 ; Compare this installar version with the existing installation
 ; and modify the messages presented to the user accordingly
 compare_version:
 StrCpy $R4 "$(older)"
 ${If} $WixMode = 1
 ReadRegStr $R0 HKLM "$R6" "DisplayVersion"
 ${Else}
 ReadRegStr $R0 SHCTX "${UNINSTKEY}" "DisplayVersion"
 ${EndIf}
 ${IfThen} $R0 == "" ${|} StrCpy $R4 "$(unknown)" ${|}

 nsis_tauri_utils::SemverCompare "${VERSION}" $R0
 Pop $R0
 ; Reinstalling the same version
 ${If} $R0 = 0
 StrCpy $R1 "$(alreadyInstalledLong)"
 StrCpy $R2 "$(addOrReinstall)"
 StrCpy $R3 "$(uninstallApp)"
 !insertmacro MUI_HEADER_TEXT "$(alreadyInstalled)" "$(chooseMaintenanceOption)"
 ; Upgrading
 ${ElseIf} $R0 = 1
 StrCpy $R1 "$(olderOrUnknownVersionInstalled)"
 StrCpy $R2 "$(uninstallBeforeInstalling)"
 StrCpy $R3 "$(dontUninstall)"
 !insertmacro MUI_HEADER_TEXT "$(alreadyInstalled)" "$(choowHowToInstall)"
 ; Downgrading
 ${ElseIf} $R0 = -1
 StrCpy $R1 "$(newerVersionInstalled)"
 StrCpy $R2 "$(uninstallBeforeInstalling)"
 !if "${ALLOWDOWNGRADES}" == "true"
 StrCpy $R3 "$(dontUninstall)"
 !else
 StrCpy $R3 "$(dontUninstallDowngrade)"
 !endif
 !insertmacro MUI_HEADER_TEXT "$(alreadyInstalled)" "$(choowHowToInstall)"
 ${Else}
 Abort
 ${EndIf}

 ; Skip showing the page if passive
 ;
 ; Note that we don't call this earlier at the begining
 ; of this function because we need to populate some variables
 ; related to current installed version if detected and whether
 ; we are downgrading or not.
 ${If} $PassiveMode = 1
 Call PageLeaveReinstall
 ${Else}
 nsDialogs::Create 1018
 Pop $R4
 ${IfThen} $(^RTL) = 1 ${|} nsDialogs::SetRTL $(^RTL) ${|}

 ${NSD_CreateLabel} 0 0 100% 24u $R1
 Pop $R1

 ${NSD_CreateRadioButton} 30u 50u -30u 8u $R2
 Pop $R2
 ${NSD_OnClick} $R2 PageReinstallUpdateSelection

 ${NSD_CreateRadioButton} 30u 70u -30u 8u $R3
 Pop $R3
 ; Disable this radio button if downgrading and downgrades are disabled
 !if "${ALLOWDOWNGRADES}" == "false"
 ${IfThen} $R0 = -1 ${|} EnableWindow $R3 0 ${|}
 !endif
 ${NSD_OnClick} $R3 PageReinstallUpdateSelection

 ; Check the first radio button if this the first time
 ; we enter this page or if the second button wasn't
 ; selected the last time we were on this page
 ${If} $ReinstallPageCheck <> 2
 SendMessage $R2 ${BM_SETCHECK} ${BST_CHECKED} 0
 ${Else}
 SendMessage $R3 ${BM_SETCHECK} ${BST_CHECKED} 0
 ${EndIf}

 ${NSD_SetFocus} $R2
 nsDialogs::Show
 ${EndIf}
FunctionEnd
Function PageReinstallUpdateSelection
 ${NSD_GetState} $R2 $R1
 ${If} $R1 == ${BST_CHECKED}
 StrCpy $ReinstallPageCheck 1
 ${Else}
 StrCpy $ReinstallPageCheck 2
 ${EndIf}
FunctionEnd
; Close a running SlideX before replacing files. Ask it to exit (taskkill
; without /F posts WM_CLOSE), wait a few seconds, and only then force-close.
; The message tells the user SlideX will close. It does not offer to kill the app.
; $R0 is saved: at reinst_uninstall it is the semver compare used to pass /UPDATE.
!macro CloseSlideXForUpdate executableName closeMessage
 !define UniqueID ${__COUNTER__}
 Push $R0
 !if "${INSTALLMODE}" == "currentUser"
  nsis_tauri_utils::FindProcessCurrentUser "${executableName}"
 !else
  nsis_tauri_utils::FindProcess "${executableName}"
 !endif
 Pop $R0
 ${If} $R0 = 0
  IfSilent slidex_close_${UniqueID} 0
  ${If} $PassiveMode != 1
   MessageBox MB_OKCANCEL "${closeMessage}" IDOK slidex_close_${UniqueID} IDCANCEL slidex_cancel_${UniqueID}
  ${EndIf}
  slidex_close_${UniqueID}:
  ; Graceful close first.
  ExecWait '"$SYSDIR\taskkill.exe" /IM "${executableName}"' $R9
  StrCpy $R8 0
  slidex_wait_${UniqueID}:
   Sleep 500
   IntOp $R8 $R8 + 1
   !if "${INSTALLMODE}" == "currentUser"
    nsis_tauri_utils::FindProcessCurrentUser "${executableName}"
   !else
    nsis_tauri_utils::FindProcess "${executableName}"
   !endif
   Pop $R0
   ${If} $R0 != 0
    Goto slidex_done_${UniqueID}
   ${EndIf}
   ${If} $R8 < 10
    Goto slidex_wait_${UniqueID}
   ${EndIf}
  ; Still running after a few seconds: force-close.
  ExecWait '"$SYSDIR\taskkill.exe" /F /T /IM "${executableName}"' $R9
  Sleep 500
  Goto slidex_done_${UniqueID}
  slidex_cancel_${UniqueID}:
  BringToFront
  Pop $R0
  Abort "SlideX is still running. Close it, then run the installer again."
  slidex_done_${UniqueID}:
 ${EndIf}
 Pop $R0
 !undef UniqueID
!macroend

Function PageLeaveReinstall
 ${NSD_GetState} $R2 $R1

 ; If migrating from Wix, always uninstall
 ${If} $WixMode = 1
 Goto reinst_uninstall
 ${EndIf}

 ; In update mode, always proceeds without uninstalling
 ${If} $UpdateMode = 1
 Goto reinst_done
 ${EndIf}

 ; $R0 holds whether same(0)/upgrading(1)/downgrading(-1) version
 ; $R1 holds the radio buttons state:
 ; 1 => first choice was selected
 ; 0 => second choice was selected
 ${If} $R0 = 0 ; Same version, proceed
 ${If} $R1 = 1 ; User chose to add/reinstall
 Goto reinst_done
 ${Else} ; User chose to uninstall
 Goto reinst_uninstall
 ${EndIf}
 ${ElseIf} $R0 = 1 ; Upgrading
 ${If} $R1 = 1 ; User chose to uninstall
 Goto reinst_uninstall
 ${Else}
 Goto reinst_done ; User chose NOT to uninstall
 ${EndIf}
 ${ElseIf} $R0 = -1 ; Downgrading
 ${If} $R1 = 1 ; User chose to uninstall
 Goto reinst_uninstall
 ${Else}
 Goto reinst_done ; User chose NOT to uninstall
 ${EndIf}
 ${EndIf}

 reinst_uninstall:
 ClearErrors

 ${If} $WixMode = 1
 ReadRegStr $R1 HKLM "$R6" "UninstallString"
 ; Close SlideX while this window is still visible. Cancel leaves it up.
 !insertmacro CloseSlideXForUpdate "${MAINBINARYNAME}.exe" "SlideX will close to finish the update."
 HideWindow
 ExecWait '$R1' $0
 ${Else}
 ReadRegStr $4 SHCTX "${MANUPRODUCTKEY}" ""
 ReadRegStr $R1 SHCTX "${UNINSTKEY}" "UninstallString"
 ; Upgrading ($R0 = 1) must pass /UPDATE so the old uninstaller cannot
 ; delete app data, even if its "delete app data" box is checked.
 ${If} $UpdateMode = 1
 ${OrIf} $R0 = 1
 StrCpy $R1 "$R1 /UPDATE"
 ${EndIf}
 ${IfThen} $PassiveMode = 1 ${|} StrCpy $R1 "$R1 /P" ${|} ; append /P
 StrCpy $R1 "$R1 _?=$4" ; append uninstall directory
 ; $R0 already decided /UPDATE above. Close now, then hide, then uninstall.
 !insertmacro CloseSlideXForUpdate "${MAINBINARYNAME}.exe" "SlideX will close to finish the update."
 HideWindow
 ExecWait '$R1' $0
 ${EndIf}

 BringToFront

 ${IfThen} ${Errors} ${|} StrCpy $0 2 ${|} ; ExecWait failed, set fake exit code

 ${If} $0 <> 0
 ${OrIf} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
 ; User cancelled wix uninstaller? return to select un/reinstall page
 ${If} $WixMode = 1
 ${AndIf} $0 = 1602
 Abort
 ${EndIf}

 ; User cancelled NSIS uninstaller? return to select un/reinstall page
 ${If} $0 = 1
 Abort
 ${EndIf}

 ; Other erros? show generic error message and return to select un/reinstall page
 MessageBox MB_ICONEXCLAMATION "$(unableToUninstall)"
 Abort
 ${EndIf}
 reinst_done:
FunctionEnd

; 5. Choose install directory page
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY

; 6. Start menu shortcut page
Var AppStartMenuFolder
!if "${STARTMENUFOLDER}" != ""
 !define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
 !define MUI_STARTMENUPAGE_DEFAULTFOLDER "${STARTMENUFOLDER}"
!else
 !define MUI_PAGE_CUSTOMFUNCTION_PRE Skip
!endif
!insertmacro MUI_PAGE_STARTMENU Application $AppStartMenuFolder

; 7. Installation page
!insertmacro MUI_PAGE_INSTFILES

; 8. Finish page
;
; Don't auto jump to finish page after installation page,
; because the installation page has useful info that can be used debug any issues with the installer.
!define MUI_FINISHPAGE_NOAUTOCLOSE
; Use show readme button in the finish page as a button create a desktop shortcut
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "$(createDesktop)"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION CreateOrUpdateDesktopShortcut
; Show run app after installation.
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION RunMainBinary
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_FINISH

Function RunMainBinary
 nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" ""
FunctionEnd

; Uninstaller Pages
; 1. Confirm uninstall page
Var DeleteAppDataCheckbox
Var DeleteAppDataCheckboxState
!define /ifndef WS_EX_LAYOUTRTL 0x00400000
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.ConfirmShow
Function un.ConfirmShow ; Add add a `Delete app data` check box
 ; Updates never offer this. Only an explicit uninstall may delete app data.
 ${If} $UpdateMode = 1
  Return
 ${EndIf}
 ; $1 inner dialog HWND
 ; $2 window DPI
 ; $3 style
 ; $4 x
 ; $5 y
 ; $6 width
 ; $7 height
 FindWindow $1 "#32770" "" $HWNDPARENT ; Find inner dialog
 System::Call "user32::GetDpiForWindow(p r1) i .r2"
 ${If} $(^RTL) = 1
 StrCpy $3 "${__NSD_CheckBox_EXSTYLE} | ${WS_EX_LAYOUTRTL}"
 IntOp $4 50 * $2
 ${Else}
 StrCpy $3 "${__NSD_CheckBox_EXSTYLE}"
 IntOp $4 0 * $2
 ${EndIf}
 IntOp $5 100 * $2
 IntOp $6 400 * $2
 IntOp $7 25 * $2
 IntOp $4 $4 / 96
 IntOp $5 $5 / 96
 IntOp $6 $6 / 96
 IntOp $7 $7 / 96
 System::Call 'user32::CreateWindowEx(i r3, w "${__NSD_CheckBox_CLASS}", w "$(deleteAppData)", i ${__NSD_CheckBox_STYLE}, i r4, i r5, i r6, i r7, p r1, i0, i0, i0) i .s'
 Pop $DeleteAppDataCheckbox
 SendMessage $HWNDPARENT ${WM_GETFONT} 0 0 $1
 SendMessage $DeleteAppDataCheckbox ${WM_SETFONT} $1 1
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.ConfirmLeave
Function un.ConfirmLeave
 ${If} $UpdateMode = 1
  StrCpy $DeleteAppDataCheckboxState 0
  Return
 ${EndIf}
 SendMessage $DeleteAppDataCheckbox ${BM_GETCHECK} 0 0 $DeleteAppDataCheckboxState
FunctionEnd
!define MUI_PAGE_CUSTOMFUNCTION_PRE un.SkipIfPassive
!insertmacro MUI_UNPAGE_CONFIRM

; 2. Uninstalling Page
!insertmacro MUI_UNPAGE_INSTFILES

;Languages
{{#each languages}}
!insertmacro MUI_LANGUAGE "{{this}}"
{{/each}}
!insertmacro MUI_RESERVEFILE_LANGDLL
{{#each language_files}}
 !include "{{this}}"
{{/each}}

Function .onInit
 ${GetOptions} $CMDLINE "/P" $PassiveMode
 ${IfNot} ${Errors}
 StrCpy $PassiveMode 1
 ${EndIf}

 ${GetOptions} $CMDLINE "/NS" $NoShortcutMode
 ${IfNot} ${Errors}
 StrCpy $NoShortcutMode 1
 ${EndIf}

 ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode
 ${IfNot} ${Errors}
 StrCpy $UpdateMode 1
 ${EndIf}

 !if "${DISPLAYLANGUAGESELECTOR}" == "true"
 !insertmacro MUI_LANGDLL_DISPLAY
 !endif

 !insertmacro SetContext

 ${If} $INSTDIR == "${PLACEHOLDER_INSTALL_DIR}"
 ; Set default install location
 !if "${INSTALLMODE}" == "perMachine"
 ${If} ${RunningX64}
 !if "${ARCH}" == "x64"
 StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
 !else if "${ARCH}" == "arm64"
 StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
 !else
 StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
 !endif
 ${Else}
 StrCpy $INSTDIR "$PROGRAMFILES\${PRODUCTNAME}"
 ${EndIf}
 !else if "${INSTALLMODE}" == "currentUser"
 StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"
 !endif

 Call RestorePreviousInstallLocation
 ${EndIf}

 !if "${INSTALLMODE}" == "both"
 !insertmacro MULTIUSER_INIT
 !endif
FunctionEnd

Section EarlyChecks
 ; Abort silent installer if downgrades is disabled
 !if "${ALLOWDOWNGRADES}" == "false"
 ${If} ${Silent}
 ; If downgrading
 ${If} $R0 = -1
 System::Call 'kernel32::AttachConsole(i -1)i.r0'
 ${If} $0 <> 0
 System::Call 'kernel32::GetStdHandle(i -11)i.r0'
 System::call 'kernel32::SetConsoleTextAttribute(i r0, i 0x0004)' ; set red color
 FileWrite $0 "$(silentDowngrades)"
 ${EndIf}
 Abort
 ${EndIf}
 ${EndIf}
 !endif

SectionEnd

Section WebView2
 ; Check if Webview2 is already installed and skip this section
 ${If} ${RunningX64}
 ReadRegStr $4 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
 ${Else}
 ReadRegStr $4 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
 ${EndIf}
 ${If} $4 == ""
 ReadRegStr $4 HKCU "SOFTWARE\Microsoft\EdgeUpdate\Clients\${WEBVIEW2APPGUID}" "pv"
 ${EndIf}

 ${If} $4 == ""
 ; Webview2 installation
 ;
 ; Skip if updating
 ${If} $UpdateMode <> 1
 !if "${INSTALLWEBVIEW2MODE}" == "downloadBootstrapper"
 Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
 DetailPrint "$(webview2Downloading)"
 NSISdl::download "https://go.microsoft.com/fwlink/p/?LinkId=2124703" "$TEMP\MicrosoftEdgeWebview2Setup.exe"
 Pop $0
 ${If} $0 == "success"
 DetailPrint "$(webview2DownloadSuccess)"
 ${Else}
 DetailPrint "$(webview2DownloadError)"
 Abort "$(webview2AbortError)"
 ${EndIf}
 StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
 Goto install_webview2
 !endif

 !if "${INSTALLWEBVIEW2MODE}" == "embedBootstrapper"
 Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
 File "/oname=$TEMP\MicrosoftEdgeWebview2Setup.exe" "${WEBVIEW2BOOTSTRAPPERPATH}"
 DetailPrint "$(installingWebview2)"
 StrCpy $6 "$TEMP\MicrosoftEdgeWebview2Setup.exe"
 Goto install_webview2
 !endif

 !if "${INSTALLWEBVIEW2MODE}" == "offlineInstaller"
 Delete "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
 File "/oname=$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe" "${WEBVIEW2INSTALLERPATH}"
 DetailPrint "$(installingWebview2)"
 StrCpy $6 "$TEMP\MicrosoftEdgeWebView2RuntimeInstaller.exe"
 Goto install_webview2
 !endif

 Goto webview2_done

 install_webview2:
 DetailPrint "$(installingWebview2)"
 ; $6 holds the path to the webview2 installer
 ExecWait "$6 ${WEBVIEW2INSTALLERARGS} /install" $1
 ${If} $1 = 0
 DetailPrint "$(webview2InstallSuccess)"
 ${Else}
 DetailPrint "$(webview2InstallError)"
 Abort "$(webview2AbortError)"
 ${EndIf}
 webview2_done:
 ${EndIf}
 ${Else}
 !if "${MINIMUMWEBVIEW2VERSION}" != ""
 ${VersionCompare} "${MINIMUMWEBVIEW2VERSION}" "$4" $R0
 ${If} $R0 = 1
 update_webview:
 DetailPrint "$(installingWebview2)"
 ${If} ${RunningX64}
 ReadRegStr $R1 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate" "path"
 ${Else}
 ReadRegStr $R1 HKLM "SOFTWARE\Microsoft\EdgeUpdate" "path"
 ${EndIf}
 ${If} $R1 == ""
 ReadRegStr $R1 HKCU "SOFTWARE\Microsoft\EdgeUpdate" "path"
 ${EndIf}
 ${If} $R1 != ""
 ; Chromium updater docs: https://source.chromium.org/chromium/chromium/src/+/main:docs/updater/user_manual.md
 ; Modified from "HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Microsoft EdgeWebView\ModifyPath"
 ExecWait `"$R1" /install appguid=${WEBVIEW2APPGUID}&needsadmin=true` $1
 ${If} $1 = 0
 DetailPrint "$(webview2InstallSuccess)"
 ${Else}
 MessageBox MB_ICONEXCLAMATION|MB_ABORTRETRYIGNORE "$(webview2InstallError)" IDIGNORE ignore IDRETRY update_webview
 Quit
 ignore:
 ${EndIf}
 ${EndIf}
 ${EndIf}
 !endif
 ${EndIf}
SectionEnd

; $R8 is a finished temp status. Replace cast-firewall.txt only after that write succeeds.
; IfFileExists offsets: +1 is the next line. A missing temp file skips both writes.
!macro CAST_FW_COMMIT
 IfFileExists "$R8" +1 +3
 Delete "$1"
 Rename /REBOOTOK "$R8" "$1"
!macroend

Function CastFirewallInstall
 ; Rules are added by one elevated cmd.exe /c, or skipped when cast-fw-check.ps1 reports a match.
 ; The pre-check is unelevated. PowerShell is the System32 binary, never PATH.
 ; A silent install (/S) never calls ExecShell runas. The live status file is replaced
 ; only after the new text is fully written to cast-firewall.new.
 Delete "$INSTDIR\cast-firewall-add.cmd"
 Delete "$INSTDIR\cast-firewall.ok"
 Delete "$TEMP\slidex-cast-firewall-remove.cmd"
 Delete "$TEMP\slidex-cast-firewall-removed.txt"
 StrCpy $1 "$INSTDIR\cast-firewall.txt"
 StrCpy $R8 "$INSTDIR\cast-firewall.new"
 Delete "$R8"
 nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$INSTDIR\cast-fw-check.ps1" -ExePath "$INSTDIR\${MAINBINARYNAME}.exe" -MediaName "${CAST_FW_MEDIA_NAME}" -MdnsName "${CAST_FW_MDNS_NAME}" -TcpPorts "${CAST_FW_TCP}" -UdpPort "${CAST_FW_UDP}"'
 Pop $0
 Pop $R5
 ${StrLoc} $0 $R5 "status=match" ">"
 StrCmp $0 "" cast_fw_need_add
 FileOpen $2 "$R8" w
 FileWrite $2 "status=present$\r$\n"
 FileWrite $2 "program=$INSTDIR\${MAINBINARYNAME}.exe$\r$\n"
 FileWrite $2 "profile=private$\r$\n"
 FileWrite $2 "tcp=${CAST_FW_TCP}$\r$\n"
 FileWrite $2 "udp=${CAST_FW_UDP}$\r$\n"
 FileClose $2
 !insertmacro CAST_FW_COMMIT
 DetailPrint "Cast firewall: both Private rules already match $INSTDIR\${MAINBINARYNAME}.exe. No Windows prompt."
 Return
cast_fw_need_add:
 IfSilent +1 cast_fw_elevate
 ${StrLoc} $0 $R5 "status=nomatch" ">"
 StrCmp $0 "" cast_fw_check_failed
 FileOpen $2 "$R8" w
 FileWrite $2 "status=skipped_silent$\r$\n"
 FileClose $2
 !insertmacro CAST_FW_COMMIT
 DetailPrint "Cast firewall: silent install left the rules unchanged. No Windows prompt."
 Goto cast_fw_install_end
cast_fw_check_failed:
 FileOpen $2 "$R8" w
 FileWrite $2 "status=check_failed$\r$\n"
 FileClose $2
 !insertmacro CAST_FW_COMMIT
 DetailPrint "Cast firewall: the pre-check did not report match or nomatch. No Windows prompt."
 Goto cast_fw_install_end
cast_fw_elevate:
 ; cmd /c strips only the first and last quote. Inner quotes are plain ", never \".
 ; The elevated command writes the temp file. This function renames it onto cast-firewall.txt.
 StrCpy $R9 '/c $\"netsh advfirewall firewall delete rule name=$\"${CAST_FW_MEDIA_NAME}$\" >nul 2>&1 & netsh advfirewall firewall delete rule name=$\"${CAST_FW_MDNS_NAME}$\" >nul 2>&1 & (netsh advfirewall firewall add rule name=$\"${CAST_FW_MEDIA_NAME}$\" dir=in action=allow protocol=TCP localport=${CAST_FW_TCP} profile=private remoteip=localsubnet program=$\"$INSTDIR\${MAINBINARYNAME}.exe$\" enable=yes && netsh advfirewall firewall add rule name=$\"${CAST_FW_MDNS_NAME}$\" dir=in action=allow protocol=UDP localport=${CAST_FW_UDP} profile=private remoteip=localsubnet program=$\"$INSTDIR\${MAINBINARYNAME}.exe$\" enable=yes && (echo status=added>$\"$INSTDIR\cast-firewall.new$\") || (echo status=failed>$\"$INSTDIR\cast-firewall.new$\"))$\"'
 DetailPrint "Cast firewall elevated command: $\"$SYSDIR\cmd.exe$\" $R9"
 ExecShell "runas" "$SYSDIR\cmd.exe" '$R9' SW_HIDE
 StrCpy $3 0
${Do}
 Sleep 500
 IfFileExists "$R8" cast_fw_install_done
 IntOp $3 $3 + 1
${LoopUntil} $3 > 90
 FileOpen $2 "$R8" w
 FileWrite $2 "status=declined$\r$\n"
 FileWrite $2 "The Windows prompt was declined or timed out.$\r$\n"
 FileWrite $2 "Cast needs Private inbound rules for $INSTDIR\${MAINBINARYNAME}.exe, TCP ${CAST_FW_TCP} and UDP ${CAST_FW_UDP}.$\r$\n"
 FileClose $2
 !insertmacro CAST_FW_COMMIT
 DetailPrint "Cast firewall: not added. See $1"
 ${IfNot} ${Silent}
  MessageBox MB_OK|MB_ICONEXCLAMATION "SlideX could not add the Private-network firewall rules.$\r$\n$\r$\nCast needs them so the TV can load photos and video. Re-run the installer and accept the Windows prompt.$\r$\n$\r$\nA per-user install cannot add a port-scoped Private rule without that prompt."
 ${EndIf}
 Goto cast_fw_install_end
cast_fw_install_done:
 !insertmacro CAST_FW_COMMIT
 DetailPrint "Cast firewall result file: $1"
 StrCpy $R7 0
 ClearErrors
 FileOpen $2 "$1" r
 ${Do}
  FileRead $2 $4
  IfErrors cast_fw_install_read_done
  DetailPrint "$4"
  ${StrLoc} $0 $4 "status=added" ">"
  StrCmp $0 "" +2
  StrCpy $R7 1
 ${Loop}
cast_fw_install_read_done:
 FileClose $2
 ${If} $R7 = 1
  Goto cast_fw_install_end
 ${EndIf}
 DetailPrint "Cast firewall: netsh did not add both Private rules. See $1"
 ${IfNot} ${Silent}
  MessageBox MB_OK|MB_ICONEXCLAMATION "SlideX could not add both Private firewall rules.$\r$\n$\r$\nSee $1$\r$\nRe-run the installer and accept the Windows prompt."
 ${EndIf}
cast_fw_install_end:
FunctionEnd

Function un.CastFirewallUninstall
 ${If} $UpdateMode = 1
  DetailPrint "Cast firewall rules kept for this upgrade. The new install refreshes them."
  Goto cast_fw_un_end
 ${EndIf}
 Delete "$INSTDIR\cast-firewall-add.cmd"
 Delete "$INSTDIR\cast-firewall.ok"
 Delete "$TEMP\slidex-cast-firewall-remove.cmd"
 Delete "$TEMP\slidex-cast-firewall-removed.txt"
 StrCpy $1 "$INSTDIR\cast-firewall.txt"
 StrCpy $R8 "$INSTDIR\cast-firewall.new"
 Delete "$R8"
 StrCpy $R9 '/c $\"netsh advfirewall firewall delete rule name=$\"${CAST_FW_MEDIA_NAME}$\" & netsh advfirewall firewall delete rule name=$\"${CAST_FW_MDNS_NAME}$\" & echo status=removed>$\"$INSTDIR\cast-firewall.new$\"$\"'
 DetailPrint "Cast firewall elevated command: $\"$SYSDIR\cmd.exe$\" $R9"
 ExecShell "runas" "$SYSDIR\cmd.exe" '$R9' SW_HIDE
 StrCpy $3 0
${Do}
 Sleep 500
 IfFileExists "$R8" cast_fw_un_done
 IntOp $3 $3 + 1
${LoopUntil} $3 > 90
 DetailPrint "Cast firewall: rules were not removed (prompt declined or timed out). Remove $\"${CAST_FW_MEDIA_NAME}$\" and $\"${CAST_FW_MDNS_NAME}$\" in Windows Firewall if they remain."
 ${IfNot} ${Silent}
  MessageBox MB_OK|MB_ICONEXCLAMATION "SlideX could not remove the Cast firewall rules.$\r$\n$\r$\nIf they remain, delete $\"${CAST_FW_MEDIA_NAME}$\" and $\"${CAST_FW_MDNS_NAME}$\" in Windows Defender Firewall."
 ${EndIf}
 Goto cast_fw_un_end
cast_fw_un_done:
 !insertmacro CAST_FW_COMMIT
 DetailPrint "Cast firewall rules removed. See $1"
 Delete "$1"
cast_fw_un_end:
 Delete "$INSTDIR\cast-firewall.new"
 Delete "$INSTDIR\cast-firewall.ok"
 Delete "$INSTDIR\cast-firewall-add.cmd"
 Delete "$INSTDIR\cast-fw-check.ps1"
 Delete "$TEMP\slidex-cast-firewall-remove.cmd"
FunctionEnd

Section Install
 SetOutPath $INSTDIR

 !ifmacrodef NSIS_HOOK_PREINSTALL
 !insertmacro NSIS_HOOK_PREINSTALL
 !endif

 !insertmacro CloseSlideXForUpdate "${MAINBINARYNAME}.exe" "SlideX will close to finish the update."

 ; Copy main executable
 File "${MAINBINARYSRCPATH}"

 ; Copy resources
 {{#each resources_dirs}}
 CreateDirectory "$INSTDIR\\{{this}}"
 {{/each}}
 {{#each resources}}
 File /a "/oname={{this.[1]}}" "{{no-escape @key}}"
 {{/each}}

 ; Copy external binaries
 {{#each binaries}}
 File /a "/oname={{this}}" "{{no-escape @key}}"
 {{/each}}

 ; Create file associations
 {{#each file_associations as |association| ~}}
 {{#each association.ext as |ext| ~}}
 !insertmacro APP_ASSOCIATE "{{ext}}" "{{or association.name ext}}" "{{association-description association.description ext}}" "$INSTDIR\${MAINBINARYNAME}.exe,0" "Open with ${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe $\"%1$\""
 {{/each}}
 {{/each}}

 ; Register deep links
 {{#each deep_link_protocols as |protocol| ~}}
 WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "URL Protocol" ""
 WriteRegStr SHCTX "Software\Classes\\{{protocol}}" "" "URL:${BUNDLEID} protocol"
 WriteRegStr SHCTX "Software\Classes\\{{protocol}}\DefaultIcon" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0"
 WriteRegStr SHCTX "Software\Classes\\{{protocol}}\shell\open\command" "" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
 {{/each}}

 ; Create uninstaller
 WriteUninstaller "$INSTDIR\uninstall.exe"

 ; Save $INSTDIR in registry for future installations
 WriteRegStr SHCTX "${MANUPRODUCTKEY}" "" $INSTDIR

 !if "${INSTALLMODE}" == "both"
 ; Save install mode to be selected by default for the next installation such as updating
 ; or when uninstalling
 WriteRegStr SHCTX "${UNINSTKEY}" $MultiUser.InstallMode 1
 !endif

 ; Remove old main binary if it doesn't match new main binary name
 ReadRegStr $OldMainBinaryName SHCTX "${UNINSTKEY}" "MainBinaryName"
 ${If} $OldMainBinaryName != ""
 ${AndIf} $OldMainBinaryName != "${MAINBINARYNAME}.exe"
 Delete "$INSTDIR\$OldMainBinaryName"
 ${EndIf}

 ; Save current MAINBINARYNAME for future updates
 WriteRegStr SHCTX "${UNINSTKEY}" "MainBinaryName" "${MAINBINARYNAME}.exe"

 ; Registry information for add/remove programs
 WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
 WriteRegStr SHCTX "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
 WriteRegStr SHCTX "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
 WriteRegStr SHCTX "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
 WriteRegStr SHCTX "${UNINSTKEY}" "InstallLocation" "$\"$INSTDIR$\""
 WriteRegStr SHCTX "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
 WriteRegDWORD SHCTX "${UNINSTKEY}" "NoModify" "1"
 WriteRegDWORD SHCTX "${UNINSTKEY}" "NoRepair" "1"

 ${GetSize} "$INSTDIR" "/M=uninstall.exe /S=0K /G=0" $0 $1 $2
 IntOp $0 $0 + ${ESTIMATEDSIZE}
 IntFmt $0 "0x%08X" $0
 WriteRegDWORD SHCTX "${UNINSTKEY}" "EstimatedSize" "$0"

 !if "${HOMEPAGE}" != ""
 WriteRegStr SHCTX "${UNINSTKEY}" "URLInfoAbout" "${HOMEPAGE}"
 WriteRegStr SHCTX "${UNINSTKEY}" "URLUpdateInfo" "${HOMEPAGE}"
 WriteRegStr SHCTX "${UNINSTKEY}" "HelpLink" "${HOMEPAGE}"
 !endif

 ; Create start menu shortcut
 !insertmacro MUI_STARTMENU_WRITE_BEGIN Application
 Call CreateOrUpdateStartMenuShortcut
 !insertmacro MUI_STARTMENU_WRITE_END

 ; Create desktop shortcut for silent and passive installers
 ; because finish page will be skipped
 ${If} $PassiveMode = 1
 ${OrIf} ${Silent}
 Call CreateOrUpdateDesktopShortcut
 ${EndIf}

 !ifmacrodef NSIS_HOOK_POSTINSTALL
 !insertmacro NSIS_HOOK_POSTINSTALL
 !endif

 ; Private-only Cast firewall rules. The resources File loop above already
 ; copied cast-fw-check.ps1 into $INSTDIR. Ask once via UAC (cmd.exe /c,
 ; no elevated script) unless both rules already match.
 Call CastFirewallInstall

 ; Auto close this page for passive mode
 ${If} $PassiveMode = 1
 SetAutoClose true
 ${EndIf}
SectionEnd

Function .onInstSuccess
 ; Check for `/R` flag only in silent and passive installers because
 ; GUI installer has a toggle for the user to (re)start the app
 ${If} $PassiveMode = 1
 ${OrIf} ${Silent}
 ${GetOptions} $CMDLINE "/R" $R0
 ${IfNot} ${Errors}
 ${GetOptions} $CMDLINE "/ARGS" $R0
 nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" "$R0"
 ${EndIf}
 ${EndIf}
FunctionEnd

Function un.onInit
 !insertmacro SetContext

 !if "${INSTALLMODE}" == "both"
 !insertmacro MULTIUSER_UNINIT
 !endif

 !insertmacro MUI_UNGETLANGUAGE

 ${GetOptions} $CMDLINE "/P" $PassiveMode
 ${IfNot} ${Errors}
 StrCpy $PassiveMode 1
 ${EndIf}

 ${GetOptions} $CMDLINE "/UPDATE" $UpdateMode
 ${IfNot} ${Errors}
 StrCpy $UpdateMode 1
 ${EndIf}
FunctionEnd

Section Uninstall

 !ifmacrodef NSIS_HOOK_PREUNINSTALL
 !insertmacro NSIS_HOOK_PREUNINSTALL
 !endif

 ${If} $UpdateMode = 1
  !insertmacro CloseSlideXForUpdate "${MAINBINARYNAME}.exe" "SlideX will close to finish the update."
 ${Else}
  !insertmacro CloseSlideXForUpdate "${MAINBINARYNAME}.exe" "SlideX will close to finish uninstalling."
 ${EndIf}

 ; Drop the Cast rules unless this uninstall is the upgrade handoff.
 ; The next install adds them again, so an upgrade does not prompt twice.
 Call un.CastFirewallUninstall

 ; Delete the app directory and its content from disk
 ; Copy main executable
 Delete "$INSTDIR\${MAINBINARYNAME}.exe"

 ; Delete resources
 {{#each resources}}
 Delete "$INSTDIR\\{{this.[1]}}"
 {{/each}}

 ; Delete external binaries
 {{#each binaries}}
 Delete "$INSTDIR\\{{this}}"
 {{/each}}

 ; Delete app associations
 {{#each file_associations as |association| ~}}
 {{#each association.ext as |ext| ~}}
 !insertmacro APP_UNASSOCIATE "{{ext}}" "{{or association.name ext}}"
 {{/each}}
 {{/each}}

 ; Delete deep links
 {{#each deep_link_protocols as |protocol| ~}}
 ReadRegStr $R7 SHCTX "Software\Classes\\{{protocol}}\shell\open\command" ""
 ${If} $R7 == "$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\""
 DeleteRegKey SHCTX "Software\Classes\\{{protocol}}"
 ${EndIf}
 {{/each}}

 ; Delete uninstaller
 Delete "$INSTDIR\uninstall.exe"

 {{#each resources_ancestors}}
 RMDir /REBOOTOK "$INSTDIR\\{{this}}"
 {{/each}}
 RMDir "$INSTDIR"

 ; Remove shortcuts if not updating
 ${If} $UpdateMode <> 1
 !insertmacro DeleteAppUserModelId

 ; Remove start menu shortcut
 !insertmacro MUI_STARTMENU_GETFOLDER Application $AppStartMenuFolder
 !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Pop $0
 ${If} $0 = 1
 !insertmacro UnpinShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
 Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
 RMDir "$SMPROGRAMS\$AppStartMenuFolder"
 ${EndIf}
 !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Pop $0
 ${If} $0 = 1
 !insertmacro UnpinShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk"
 Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
 ${EndIf}

 ; Remove desktop shortcuts
 !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Pop $0
 ${If} $0 = 1
 !insertmacro UnpinShortcut "$DESKTOP\${PRODUCTNAME}.lnk"
 Delete "$DESKTOP\${PRODUCTNAME}.lnk"
 ${EndIf}
 ${EndIf}

 ; Remove registry information for add/remove programs
 !if "${INSTALLMODE}" == "both"
 DeleteRegKey SHCTX "${UNINSTKEY}"
 !else if "${INSTALLMODE}" == "perMachine"
 DeleteRegKey HKLM "${UNINSTKEY}"
 !else
 DeleteRegKey HKCU "${UNINSTKEY}"
 !endif

 ; Removes the Autostart entry for ${PRODUCTNAME} from the HKCU Run key if it exists.
 ; This ensures the program does not launch automatically after uninstallation if it exists.
 ; If it doesn't exist, it does nothing.
 ; We do this when not updating (to preserve the registry value on updates)
 ${If} $UpdateMode <> 1
 DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
 ${EndIf}

 ; Delete app data only for an explicit uninstall with the checkbox checked.
 ; Never during an update (/UPDATE), including uninstall-before-install.
 ; This is the only place that removes:
 ;   %LOCALAPPDATA%\${BUNDLEID}
 ;   %APPDATA%\${BUNDLEID}
 ;   and the WebView2 EBWebView data inside the local folder.
 ${If} $UpdateMode = 1
  StrCpy $DeleteAppDataCheckboxState 0
 ${EndIf}
 ${If} $DeleteAppDataCheckboxState = 1
 ${AndIf} $UpdateMode <> 1
 ; Clear the install location $INSTDIR from registry
 DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
 DeleteRegKey /ifempty SHCTX "${MANUKEY}"

 ; Clear the install language from registry
 DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
 DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
 DeleteRegKey /ifempty HKCU "${MANUKEY}"

 SetShellVarContext current
 RmDir /r "$APPDATA\${BUNDLEID}"
 RmDir /r "$LOCALAPPDATA\${BUNDLEID}"
 ${EndIf}

 !ifmacrodef NSIS_HOOK_POSTUNINSTALL
 !insertmacro NSIS_HOOK_POSTUNINSTALL
 !endif

 ; Auto close if passive mode or updating
 ${If} $PassiveMode = 1
 ${OrIf} $UpdateMode = 1
 SetAutoClose true
 ${EndIf}
SectionEnd

Function RestorePreviousInstallLocation
 ReadRegStr $4 SHCTX "${MANUPRODUCTKEY}" ""
 StrCmp $4 "" +2 0
 StrCpy $INSTDIR $4
FunctionEnd

Function Skip
 Abort
FunctionEnd

Function SkipIfPassive
 ${IfThen} $PassiveMode = 1 ${|} Abort ${|}
FunctionEnd
Function un.SkipIfPassive
 ${IfThen} $PassiveMode = 1 ${|} Abort ${|}
FunctionEnd

Function CreateOrUpdateStartMenuShortcut
 ; 0.1.7 display rename: replace a 0.1.6 SlideShowX shortcut that still targets this install.
 !insertmacro IsShortcutTarget "$SMPROGRAMS\SlideShowX.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Pop $0
 ${If} $0 = 1
  !insertmacro UnpinShortcut "$SMPROGRAMS\SlideShowX.lnk"
  Delete "$SMPROGRAMS\SlideShowX.lnk"
  CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
 ${EndIf}
 !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\SlideShowX.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Pop $0
 ${If} $0 = 1
  !insertmacro UnpinShortcut "$SMPROGRAMS\$AppStartMenuFolder\SlideShowX.lnk"
  Delete "$SMPROGRAMS\$AppStartMenuFolder\SlideShowX.lnk"
  CreateDirectory "$SMPROGRAMS\$AppStartMenuFolder"
  CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
 ${EndIf}

 ; We used to use product name as MAINBINARYNAME
 ; migrate old shortcuts to target the new MAINBINARYNAME
 StrCpy $R0 0

 !insertmacro IsShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
 Pop $0
 ${If} $0 = 1
 !insertmacro SetShortcutTarget "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 StrCpy $R0 1
 ${EndIf}

 !insertmacro IsShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
 Pop $0
 ${If} $0 = 1
 !insertmacro SetShortcutTarget "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 StrCpy $R0 1
 ${EndIf}

 ${If} $R0 = 1
 Return
 ${EndIf}

 ; Skip creating shortcut if in update mode or no shortcut mode
 ; but always create if migrating from wix
 ${If} $WixMode = 0
 ${If} $UpdateMode = 1
 ${OrIf} $NoShortcutMode = 1
 Return
 ${EndIf}
 ${EndIf}

 !if "${STARTMENUFOLDER}" != ""
 CreateDirectory "$SMPROGRAMS\$AppStartMenuFolder"
 CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME}.lnk"
 !else
 CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
 !endif
FunctionEnd

Function CreateOrUpdateDesktopShortcut
 ; 0.1.7 display rename: replace a 0.1.6 SlideShowX shortcut that still targets this install.
 !insertmacro IsShortcutTarget "$DESKTOP\SlideShowX.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Pop $0
 ${If} $0 = 1
  !insertmacro UnpinShortcut "$DESKTOP\SlideShowX.lnk"
  Delete "$DESKTOP\SlideShowX.lnk"
  CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
 ${EndIf}

 ; We used to use product name as MAINBINARYNAME
 ; migrate old shortcuts to target the new MAINBINARYNAME
 !insertmacro IsShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\$OldMainBinaryName"
 Pop $0
 ${If} $0 = 1
 !insertmacro SetShortcutTarget "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 Return
 ${EndIf}

 ; Skip creating shortcut if in update mode or no shortcut mode
 ; but always create if migrating from wix
 ${If} $WixMode = 0
 ${If} $UpdateMode = 1
 ${OrIf} $NoShortcutMode = 1
 Return
 ${EndIf}
 ${EndIf}

 CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
 !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
FunctionEnd
